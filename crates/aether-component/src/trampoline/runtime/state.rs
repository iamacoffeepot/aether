use std::collections::VecDeque;
use std::mem;
use std::sync::Arc;

use aether_kinds::{ComponentCapabilities, DropResult};
use aether_substrate::InboundMail;
use aether_substrate::actor::native::envelope::Envelope;
use aether_substrate::actor::native::{Held, NativeCtx};
use aether_substrate::actor::wasm::component::{Component, ComponentCtx, HookFault, StateBundle};
use aether_substrate::actor::wasm::module::{Module, ModuleCache};
use aether_substrate::mail::MailId;
use aether_substrate::mail::outbound::HubOutbound;
use wasmtime::{Engine, Linker};

use crate::trampoline::WasmTrampoline;

/// Per-component trampoline **runtime state** (ADR-0122 identity/runtime
/// split — the addressing identity is the distinct ZST
/// [`WasmTrampoline`]). Holds the wasm guest in a `Slot`: live, prepared
/// beside a candidate during a republish (ADR-0241 §7), or released. A
/// `DropComponent` closes the trampoline, whose close releases the guest, so
/// its name tombstones (ADR-0241 §8).
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
    /// The drop requests this instance owes an answer. The close answers
    /// each `Ok` once the guest is released, so a requester that reads
    /// `DropResult::Ok` reads a released guest. More than one stands when a
    /// second drop was forwarded before the first one's close ran.
    pub(crate) drops: Vec<Held<DropResult>>,
}

/// Where the trampoline's guest stands (ADR-0241 §7).
pub enum Slot {
    /// One guest runs and receives its mail.
    Live(Box<Component>),
    /// A republish prepared a candidate beside the running guest, which is
    /// kept, dehydrated, with the state it saved, until a commit installs
    /// the candidate or an abort reinstates it and hands that state back.
    Prepared(Box<PreparedSlot>),
    /// No guest is resident: the transient a republish step holds while it
    /// moves a guest, the state the trampoline's close leaves, and the state
    /// a reinstatement leaves, ahead of that close, when the kept guest
    /// refused its own saved state (ADR-0249 §4).
    Released,
}

/// A republish member between prepare and commit or abort: the guest kept
/// for an abort, the candidate built, rehydrated and wired for a commit,
/// what the trampoline records once the candidate commits, and the mail its
/// inbox gate queued meanwhile.
pub struct PreparedSlot {
    /// The guest that ran until prepare. It has run `on_dehydrate`, which
    /// returned `Ok`, and its reply table, correlation cursor and watches
    /// moved to the candidate. An abort reinstates it with [`Self::saved`].
    pub(crate) old: Component,
    /// The state the kept guest saved in `on_dehydrate`, which the candidate
    /// rehydrated from. An abort hands it back to the kept guest through its
    /// `on_rehydrate`, so what the dehydrate moved out returns to it; a kept
    /// guest that returns an error from it closes.
    pub(crate) saved: Option<StateBundle>,
    /// The candidate, whose outbox is held: nothing it sent has left. Its
    /// `wire` returned `Ok`, so an abort and a close while prepared run its
    /// `unwire`.
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

/// Whether a candidate that lost owes `unwire` (ADR-0249 §4): what wired
/// unwires.
pub enum CandidateUnwire {
    /// Its `wire` ran and returned, with `Ok` or with an error.
    Owed,
    /// Its `wire` never ran, or trapped, after which it runs no more code.
    NotOwed,
}

/// A prepare the candidate refused after the old guest's hooks ran.
pub struct CandidateRefusal {
    /// What the republish is refused with.
    pub(crate) error: String,
    /// Whether the reinstatement runs the candidate's `unwire`.
    pub(crate) unwire: CandidateUnwire,
}

impl WasmTrampolineState {
    /// Run a guest's `wire` hook and publish the inline-child aliases it
    /// staged. A birth runs it once (ADR-0249 §3).
    ///
    /// A fault is the caller's to act on (ADR-0247 rule 3): a birth fails
    /// with it. The aliases a faulted guest staged are not published; they
    /// go with the guest.
    pub(crate) fn wire_guest(
        ctx: &mut NativeCtx<'_, WasmTrampoline>,
        component: &mut Component,
        root: Option<MailId>,
    ) -> Result<(), HookFault> {
        let wired = component.wire(root);
        // ADR-0163 §3 (#3984): the asset load window closes when `wire`
        // returns — it lets go of the module's code so
        // `asset_fetch_p32` traps thereafter, retaining the catalog
        // metadata for the instance's life. Runs whether or not `wire`
        // faulted; the window's job (init + wire) is done either way.
        component.close_load_window();
        wired?;
        Self::stage_guest_aliases(ctx, component);
        Ok(())
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
    /// §7), shared by a refused prepare and an abort. The candidate is
    /// unwired exactly when `unwire` says it owes it (ADR-0249 §4); its held mail is discarded, and its staged
    /// aliases drop with it, so nothing it did leaves. The reply table and
    /// correlation cursor it took over move back, past every id it minted,
    /// and so do the watches, less the ones it added (ADR-0079 §8). `old`
    /// then gets back the state its `on_dehydrate` saved, `saved`, through
    /// its own `on_rehydrate`, so a value the dehydrate moved out, a held
    /// reply among it, returns to it (ADR-0016 §4), the context of each watch
    /// among it. The old guest never runs `wire` again: its `unwire` never
    /// ran, so it is still wired. Then it receives the mail its gate queued,
    /// in order, a departure notice for a watched actor that closed meanwhile
    /// among it. Only teardown outside that saved state and outside what
    /// `wire` built stays gone.
    ///
    /// A trap in the old guest's `on_rehydrate` aborts the substrate
    /// (ADR-0063), as a trap in delivery does: there is no other guest to
    /// fall back to.
    ///
    /// The old guest's `on_rehydrate` returning an error closes the instance
    /// (ADR-0249 §4): it is intact, and it has said it cannot take its state
    /// back, with no operation left to refuse. It is not put back in
    /// `Slot::Live`, whose close would run `unwire` on a guest that never
    /// unwired at prepare. Each reply it holds is answered `unanswered`, it
    /// is dropped with the slot left `Released`, the gated mail drops and each
    /// chain settles, and the trampoline is asked to shut down. The close
    /// that follows finds no guest, clears the accept set, answers any drop
    /// request, and the name tombstones.
    pub(crate) fn reinstate(
        &mut self,
        ctx: &mut NativeCtx<'_, WasmTrampoline>,
        mut old: Component,
        mut candidate: Component,
        saved: Option<StateBundle>,
        gated: VecDeque<InboundMail>,
        unwire: CandidateUnwire,
    ) {
        match unwire {
            CandidateUnwire::Owed => candidate.unwire(),
            CandidateUnwire::NotOwed => {}
        }
        candidate.discard_held_outbox();
        old.resume_replies(candidate.take_pending_replies());
        old.resume_correlations(candidate.correlation_cursor());
        old.resume_watches(candidate.take_watches());
        drop(candidate);
        // A watch of the kept guest that still waited for its watcher's
        // alias when the table moved registers now if that alias was
        // published while the slot was prepared.
        old.register_published_watches();

        let restored = saved.map_or(Ok(()), |bundle| old.call_on_rehydrate(&bundle));
        match restored {
            Ok(()) => {}
            Err(HookFault::Trapped(trap)) => ctx.fatal_abort(format!(
                "component {} trapped restoring its state after an aborted republish: {trap:#}",
                ctx.path()
            )),
            Err(HookFault::Returned(error)) => {
                tracing::error!(
                    target: "aether_component",
                    actor = %ctx.path(),
                    %error,
                    "the guest refused its own saved state after an aborted republish; the instance closes",
                );
                old.answer_held_at_close();
                drop(old);
                drop(gated);
                ctx.shutdown();
                return;
            }
        }
        self.slot = Slot::Live(Box::new(old));
        self.release_gated(ctx, gated);
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

    /// Release the **wasm guest** as its trampoline closes: the one way a
    /// guest leaves its slot for good, whichever exit closes the trampoline
    /// (ADR-0247 rule 5).
    ///
    /// A live guest runs its `unwire` pre-shutdown hook, the single one
    /// (ADR-0079 amended), and then each reply it still holds is answered
    /// with the `unanswered` value it registered (ADR-0243 §6): closing
    /// saves no guest state, so no ticket survives to answer a held slot.
    /// The answers come after `unwire`, which answers nothing at commit
    /// since its rows already moved (ADR-0249 §7). Engine teardown answers
    /// nothing. The `Component` then drops, tearing down linear memory.
    ///
    /// A prepared slot unwires the candidate, which wired, discards
    /// its held outbox, which restores any slot a held answer reserved, moves
    /// the reply table the candidate took over back to the kept guest, runs
    /// the kept guest's `unwire`, and answers from there. The kept guest gets
    /// no `on_rehydrate` first: it is closing, and `unwire` after
    /// `on_dehydrate` is the order every replaced guest runs (ADR-0249 §4).
    /// The gated mail drops, and each chain settles as it does.
    ///
    /// Every watch the mailbox holds is released as its guest drops
    /// (ADR-0079 §8): the watch table goes with the `Component` that holds
    /// it, the candidate's in a prepared slot, and each registration with
    /// it, whether or not the guest's `unwire` ran or trapped.
    ///
    /// A trap in the guest's `unwire` is logged where it is caught and the
    /// close goes on, so the held replies still settle and the name still
    /// ends.
    ///
    /// Then the now-empty slot is synced, so the accept set clears
    /// (iamacoffeepot/aether#1037) and the released guest's cost cells leave
    /// the global table and the per-actor cache together
    /// (iamacoffeepot/aether#1128): the close runs on the trampoline's own
    /// thread inside `with_stamped`.
    ///
    /// Last, each drop request that asked for this close is answered `Ok`.
    /// The answer comes after everything above, so it still means the guest
    /// is released, as it did when the drop handler released the guest
    /// itself.
    pub(crate) fn close_guest(&mut self, ctx: &mut NativeCtx<'_, WasmTrampoline>) {
        match mem::replace(&mut self.slot, Slot::Released) {
            Slot::Live(mut component) => {
                component.unwire();
                component.answer_held_at_close();
            }
            Slot::Prepared(prepared) => {
                let PreparedSlot { mut old, mut candidate, .. } = *prepared;
                candidate.unwire();
                candidate.discard_held_outbox();
                old.resume_replies(candidate.take_pending_replies());
                old.unwire();
                old.answer_held_at_close();
            }
            Slot::Released => {}
        }

        ctx.sync_guest(self);

        for held in self.drops.drain(..) {
            held.answer(ctx, &DropResult::Ok);
        }
    }
}
