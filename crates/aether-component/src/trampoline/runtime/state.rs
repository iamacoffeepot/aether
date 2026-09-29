use std::collections::VecDeque;
use std::mem;
use std::sync::Arc;

use aether_kinds::ComponentCapabilities;
use aether_substrate::InboundMail;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::actor::native::envelope::Envelope;
use aether_substrate::actor::wasm::component::{Component, ComponentCtx};
use aether_substrate::actor::wasm::module::{Module, ModuleCache};
use aether_substrate::mail::outbound::HubOutbound;
use wasmtime::{Engine, Linker};

use crate::trampoline::WasmTrampoline;

/// Per-component trampoline **runtime state** (ADR-0122 identity/runtime
/// split — the addressing identity is the distinct ZST
/// [`WasmTrampoline`]). Holds the wasm guest in a `Slot`: live, prepared
/// beside a candidate during a republish (ADR-0241 §7), or released. A
/// `DropComponent` releases the guest and closes the trampoline, so its name
/// tombstones (ADR-0241 §8).
///
/// Its fields are crate-private, so no crate outside `aether-component` can
/// build one to hand [`NativeCtx::sync_guest`].
pub struct WasmTrampolineState {
    /// The guest this trampoline hosts, and during a republish the candidate
    /// prepared beside it.
    pub(crate) slot: Slot,
    /// Held so a republish's prepare can instantiate a candidate against the
    /// same engine + linker.
    pub(crate) engine: Arc<Engine>,
    pub(crate) linker: Arc<Linker<ComponentCtx>>,
    pub(crate) outbound: Arc<HubOutbound>,
    /// The receive surface of the guest this slot hosts, or last hosted:
    /// what [`GuestHost::guest`](aether_substrate::actor::native::ctx::GuestHost::guest)
    /// reads while the slot holds a guest. A prepared candidate's surface
    /// waits in `Slot::Prepared` until commit.
    pub(crate) capabilities: ComponentCapabilities,
    /// ADR-0096: the selected export's actor-type tag, or `None`
    /// for the entry type. Held so a republish re-instantiates the same
    /// exported type from the new wasm and re-reads that type's capability
    /// group.
    pub(crate) type_tag: Option<u64>,
    /// The init config the running guest was built with: its spawn config,
    /// or the config its last committed republish supplied. A republish that
    /// supplies none builds its candidate from this (ADR-0241 §7).
    pub(crate) config: Vec<u8>,
    /// The resident [`Module`], retained so a replace checks its replacement
    /// against the resident manifest (ADR-0231 §5), and refreshed on
    /// commit. A cheap clone of the engine module cache's entry, which it
    /// keeps alive (ADR-0240 D5, ADR-0241 §2).
    pub(crate) module: Module,
    /// The engine's one module cache: a replace checks its replacement in
    /// here, so a replacement already live anywhere in the engine is not
    /// compiled again.
    pub(crate) modules: ModuleCache,
}

/// Where the trampoline's guest stands (ADR-0241 §7).
pub enum Slot {
    /// One guest runs and receives its mail.
    Live(Box<Component>),
    /// A republish prepared a candidate beside the running guest, which is
    /// kept, unwired and dehydrated, until a commit installs the candidate or
    /// an abort reinstates it.
    Prepared(Box<PreparedSlot>),
    /// The guest is released; mail to the trampoline warn-drops.
    Released,
}

/// A republish member between prepare and commit or abort: the guest kept
/// for an abort, the candidate built and rehydrated for a commit, what the
/// trampoline records once the candidate commits, and the mail its inbox
/// gate queued meanwhile.
pub struct PreparedSlot {
    /// The guest that ran until prepare. It has run `unwire` and
    /// `on_dehydrate`, and its reply table and correlation cursor moved to
    /// the candidate.
    pub(crate) old: Component,
    /// The candidate, whose outbox is held: nothing it sent has left.
    pub(crate) candidate: Component,
    /// The candidate's module, which becomes resident on commit.
    pub(crate) module: Module,
    /// The candidate's actor-type tag.
    pub(crate) type_tag: Option<u64>,
    /// The candidate's receive surface, registered on commit.
    pub(crate) capabilities: ComponentCapabilities,
    /// The config the candidate was built with, stored on commit.
    pub(crate) config: Vec<u8>,
    /// Mail for the guest that arrived while prepared, in arrival order.
    /// Each chain stays open until the mail is delivered to the guest that
    /// wins.
    pub(crate) gated: VecDeque<InboundMail>,
}

impl WasmTrampolineState {
    /// Run a guest's `wire` hook and publish the inline-child aliases it
    /// staged. A birth runs it once, and a reinstated guest runs it again
    /// (ADR-0241 §7), since its `unwire` ran at prepare.
    pub(crate) fn wire_guest(ctx: &mut NativeCtx<'_, WasmTrampoline>, component: &mut Component) {
        if let Err(e) = component.wire() {
            tracing::error!(
                target: "aether_component",
                error = %e,
                "wasm guest `wire` hook returned error",
            );
        }
        // ADR-0163 §3 (#3984): the asset load window closes when `wire`
        // returns — it lets go of the asset blobs so
        // `asset_fetch_p32` traps thereafter, retaining the catalog
        // metadata for the instance's life. Runs whether or not `wire`
        // errored; the window's job (init + wire) is done either way.
        component.close_load_window();
        Self::stage_guest_aliases(ctx, component);
    }

    /// Deliver one envelope to a guest, then publish the inline-child
    /// aliases and retirements it staged during the call.
    pub(crate) fn deliver_to_guest(ctx: &mut NativeCtx<'_, WasmTrampoline>, component: &mut Component, env: &Envelope) {
        // The routed envelope already carries the recipient (the
        // inline-child alias when one was addressed, ADR-0114 §2) and
        // the inbound lineage (#722) that `deliver` threads to the guest.
        if let Err(e) = component.deliver(env) {
            // ADR-0063 fail-fast: a wasm trap (or host-fn error
            // returned through `Component::deliver`) kills the
            // substrate. Wedge detection (CPU-loop guests) waits
            // on a future epoch-deadline ADR — symmetric with
            // native actors, which have no wedge guard either
            // today.
            let kind = ctx.kind_label(env.kind);
            ctx.fatal_abort(format!("component {} (kind {kind}) trapped: {e}", ctx.path()));
        }
        Self::stage_guest_aliases(ctx, component);
    }

    /// Publish the inline-child aliases, and retire the ones, a guest staged
    /// during its last call.
    fn stage_guest_aliases(ctx: &mut NativeCtx<'_, WasmTrampoline>, component: &mut Component) {
        Self::stage_inline_aliases(ctx, component.drain_pending_aliases());
        Self::stage_inline_alias_retirements(ctx, component.drain_pending_alias_retirements());
    }

    /// Put `old` back as the live guest after its candidate lost (ADR-0241
    /// §7), shared by a refused prepare and an abort. The candidate's held
    /// mail is discarded, and its staged aliases drop with it, so nothing it
    /// did leaves. The reply table and correlation cursor it took over move
    /// back, past every id it minted. `old` runs `wire` again, since its
    /// `unwire` ran at prepare, and then receives the mail its gate queued,
    /// in order. Whatever `unwire` and `on_dehydrate` tore down beyond what
    /// `wire` rebuilds stays gone, as ADR-0016 §4 accepts.
    pub(crate) fn reinstate(
        &mut self,
        ctx: &mut NativeCtx<'_, WasmTrampoline>,
        mut old: Component,
        mut candidate: Component,
        gated: VecDeque<InboundMail>,
    ) {
        candidate.discard_held_outbox();
        old.resume_replies(candidate.take_pending_replies());
        old.resume_correlations(candidate.correlation_cursor());
        drop(candidate);

        Self::wire_guest(ctx, &mut old);
        self.slot = Slot::Live(Box::new(old));
        self.release_gated(ctx, gated);
    }

    /// Answer the replies the guest still holds as its trampoline closes
    /// (ADR-0243 §6), each with the `unanswered` value the guest registered.
    /// No guest code runs: the guest's own `unwire` export does not, and a
    /// prepared candidate is discarded without an abort, which would wire
    /// the kept guest again and deliver its gated mail inside the close.
    /// Instead the candidate's held outbox is discarded, which restores any
    /// slot a held answer reserved, and the reply table it took over moves
    /// back to the kept guest; the gated mail drops, and each chain settles
    /// as it does. Engine teardown answers nothing. A guest already released
    /// answered at its release.
    pub(crate) fn answer_held_at_close(&mut self) {
        self.slot = match mem::replace(&mut self.slot, Slot::Released) {
            Slot::Prepared(prepared) => {
                let PreparedSlot { mut old, mut candidate, .. } = *prepared;
                candidate.discard_held_outbox();
                old.resume_replies(candidate.take_pending_replies());
                Slot::Live(Box::new(old))
            }
            other => other,
        };
        if let Slot::Live(component) = &mut self.slot {
            component.answer_held_at_close();
        }
    }

    /// Deliver the mail a prepared slot's gate queued, in arrival order, to
    /// the guest now live. Each mail's chain closes once it is delivered.
    pub(crate) fn release_gated(&mut self, ctx: &mut NativeCtx<'_, WasmTrampoline>, gated: VecDeque<InboundMail>) {
        let Slot::Live(component) = &mut self.slot else {
            return;
        };
        for mail in gated {
            Self::deliver_to_guest(ctx, component, mail.envelope());
        }
    }

    /// Release the **wasm guest**: run its `unwire` pre-shutdown hook, answer
    /// each reply it still holds with its registered `unanswered` value
    /// (ADR-0243 §6), drop the `Component`, and sync the now-empty slot, so
    /// the accept set clears and only the framework cost cells stay. A
    /// prepared candidate is discarded first and the kept guest reinstated,
    /// so the guest released is the one that ran. The caller, a
    /// `DropComponent`, then closes the trampoline.
    pub fn release_guest(&mut self, ctx: &mut NativeCtx<'_, WasmTrampoline>) {
        if matches!(self.slot, Slot::Prepared(_)) {
            self.abort(ctx);
        }
        if let Slot::Live(mut component) = mem::replace(&mut self.slot, Slot::Released) {
            // Issue 584 Phase 3 (ADR-0079 amended): unwire is the
            // single pre-shutdown hook — the legacy `on_drop`
            // retired alongside `WasmActor::on_drop`. Component
            // drops at end of scope, tearing down linear memory.
            component.unwire();
            // #6409: after `unwire`, which may still answer handles.
            // ADR-0243 §6: releasing saves no guest state, so no ticket
            // survives to answer a held slot; each requester receives the
            // `unanswered` value its guest registered when it held.
            component.answer_held_at_close();
        }
        // The slot is empty now, so the declaration reads `None` and the sync
        // releases the guest. iamacoffeepot/aether#1037: the mailbox accepts
        // nothing more. iamacoffeepot/aether#1128: the released guest's cost
        // cells leave the global table and the per-actor cache together,
        // because the release runs on the trampoline's own thread inside
        // `with_stamped`.
        //
        // The trampoline's own framework arms are re-seeded rather than
        // dropped with them (iamacoffeepot/aether#4269): the releasing
        // handler folds into its cell just after it returns, and the closing
        // trampoline dispatches until its inbox drains. The re-seed is
        // neutral, which is the honest reading of an estimate whose occupant
        // just changed.
        ctx.sync_guest(self);
    }
}
