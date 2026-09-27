use std::sync::Arc;

use aether_kinds::ComponentCapabilities;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::actor::wasm::component::{Component, ComponentCtx, CorrelationCursor, PendingReplies};
use aether_substrate::actor::wasm::module::{Module, ModuleCache};
use aether_substrate::mail::outbound::HubOutbound;
use wasmtime::{Engine, Linker};

use crate::trampoline::WasmTrampoline;

/// Per-component trampoline **runtime state** (ADR-0122 identity/runtime
/// split — the addressing identity is the distinct ZST
/// [`WasmTrampoline`]). Holds the wasm
/// `Component` optionally — `None` means the wasm has been unloaded by
/// `DropComponent` but the trampoline (and its mailbox name) is
/// still alive, ready to be refilled by `ReplaceComponent` or
/// recycled by a future load. Distinction matters: dropping the
/// **component** is a wasm unload that preserves the addressable
/// name; dropping the **trampoline** would kill the actor and
/// tombstone the subname. The cap's `DropComponent` handler does
/// the former; the latter happens at substrate teardown.
///
/// Its fields are crate-private, so no crate outside `aether-component` can
/// build one to hand [`NativeCtx::sync_guest`].
pub struct WasmTrampolineState {
    /// `Some` while wasm is loaded; `None` after a `DropComponent`.
    /// Mail arriving in the `None` state warn-drops via the
    /// fallback (the trampoline is just an empty named slot).
    pub(crate) component: Option<Component>,
    /// Held for [`Self::handle_replace`] so a fresh
    /// `Component::instantiate` against the same engine + linker
    /// is reachable from the handler.
    pub(crate) engine: Arc<Engine>,
    pub(crate) linker: Arc<Linker<ComponentCtx>>,
    pub(crate) outbound: Arc<HubOutbound>,
    /// The receive surface of the guest this slot hosts, or last hosted:
    /// what [`GuestHost::guest`](aether_substrate::actor::native::ctx::GuestHost::guest)
    /// reads while `component` is `Some`.
    pub(crate) capabilities: ComponentCapabilities,
    /// ADR-0096: the selected export's actor-type tag, or `None`
    /// for the entry type. Held so [`Self::handle_replace`]
    /// re-instantiates the same exported type from the new wasm
    /// and re-reads that type's capability group.
    pub(crate) type_tag: Option<u64>,
    /// The resident [`Module`], retained so a sibling spawn re-instantiates
    /// its compiled code and reads its own capability group from the
    /// manifest (ADR-0097) and opens its own asset load window over the
    /// module's asset blobs (ADR-0163 §3), and refreshed on replace. A cheap
    /// clone of the engine module cache's entry, which it keeps alive
    /// (ADR-0240 D5, ADR-0241 §2).
    pub(crate) module: Module,
    /// The engine's one module cache: a replace checks its replacement in
    /// here, so a replacement already live anywhere in the engine is not
    /// compiled again.
    pub(crate) modules: ModuleCache,
    /// ADR-0139 §3 (#6400, #6422): the correlation cursor of the last guest
    /// to leave this slot, which the next occupant resumes, so the mailbox's
    /// request ids and reply-lineage ids both stay monotonic across replace
    /// and refill. `None` until a guest first leaves; a fresh slot starts
    /// both counters at their bases.
    pub(crate) retired_correlations: Option<CorrelationCursor>,
    /// #6409: the reply table of the last guest to leave this slot, which
    /// the next occupant to start resumes, so a handle issued before the
    /// swap still answers its own requester and no number is reissued.
    /// Left in place when a replacement fails to start, for the next
    /// refill. `None` until a guest first leaves, or once resumed.
    pub(crate) retired_replies: Option<PendingReplies>,
}

impl WasmTrampolineState {
    /// Unload the **wasm component**: run the guest's `unwire` pre-shutdown
    /// hook, drop the `Component`, sync the now-empty slot (the accept set
    /// clears and only the framework cost cells stay), and vacate the mailbox. The
    /// trampoline itself stays alive as an empty slot a `ReplaceComponent`
    /// can refill. Both a `DropComponent` and the host's module-boot
    /// `BootTeardown` end here.
    pub fn unload(&mut self, ctx: &mut NativeCtx<'_, WasmTrampoline>) {
        if let Some(mut component) = self.component.take() {
            // Issue 584 Phase 3 (ADR-0079 amended): unwire is the
            // single pre-shutdown hook — the legacy `on_drop`
            // retired alongside `WasmActor::on_drop`. Component
            // drops at end of scope, tearing down linear memory.
            component.unwire();
            // #6400: after `unwire`, which may still send, so a later
            // refill resumes past every id this guest minted.
            self.retired_correlations = Some(component.correlation_cursor());
            // #6409: after `unwire`, which may still answer handles, so a
            // later refill answers the rest to their own requesters.
            self.retired_replies = Some(component.take_pending_replies());
        }
        // The slot is empty now, so the declaration reads `None` and the sync
        // releases the guest. iamacoffeepot/aether#1037: the mailbox accepts
        // nothing until a `replace` refills it; the trampoline (and its
        // mailbox name) survives as an empty slot with no accept set.
        // iamacoffeepot/aether#1128: the unloaded guest's cost cells leave
        // the global table and the per-actor cache together, because `unload`
        // runs on the trampoline's own thread inside `with_stamped`.
        //
        // The trampoline's own framework arms are re-seeded rather than
        // dropped with them (iamacoffeepot/aether#4269): the mailbox survives
        // this as an empty refillable slot and goes on dispatching
        // `ReplaceComponent`, `DropComponent` and its task wakes, so retiring
        // their cells left the arms that outlive the guest unmeasured — the
        // unloading handler among them, which folds into its cell just after
        // it returns. The re-seed is neutral, which is the honest reading of
        // an estimate whose occupant just changed.
        ctx.sync_guest(self);
        // ADR-0079 §8 (amended, issue 3741): declare the mailbox
        // vacated — drain this trampoline's watchers and fire one
        // `MonitorNotice` each, so every cap holding state keyed by
        // this mailbox (input subscriptions, lifecycle stages, http
        // routes) purges its own rows. The slot stays live for a
        // `replace` refill; the next occupant's watchers register
        // fresh.
        ctx.vacate();
    }
}
