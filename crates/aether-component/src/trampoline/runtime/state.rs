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
/// [`WasmTrampoline`]). Holds the wasm `Component` optionally: `None` once
/// the guest is released. A `DropComponent` releases the guest and closes
/// the trampoline, so its name tombstones (ADR-0241 §8); the host's
/// module-boot `BootTeardown` releases the guest and vacates the mailbox,
/// leaving an empty slot until the substrate stops.
///
/// Its fields are crate-private, so no crate outside `aether-component` can
/// build one to hand [`NativeCtx::sync_guest`].
pub struct WasmTrampolineState {
    /// `Some` while wasm is loaded; `None` once the guest is released.
    /// Mail arriving in the `None` state warn-drops via the fallback.
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
    /// The resident [`Module`], retained so a replace checks its replacement
    /// against the resident manifest (ADR-0231 §5), and refreshed on
    /// replace. A cheap clone of the engine module cache's entry, which it
    /// keeps alive (ADR-0240 D5, ADR-0241 §2).
    pub(crate) module: Module,
    /// The engine's one module cache: a replace checks its replacement in
    /// here, so a replacement already live anywhere in the engine is not
    /// compiled again.
    pub(crate) modules: ModuleCache,
    /// ADR-0139 §3 (#6400, #6422): the correlation cursor of the last guest
    /// to leave this slot, which the next occupant resumes, so the mailbox's
    /// request ids and reply-lineage ids both stay monotonic across replace.
    /// `None` until a guest first leaves; a fresh slot starts both counters
    /// at their bases.
    pub(crate) retired_correlations: Option<CorrelationCursor>,
    /// #6409: the reply table of the last guest to leave this slot, which
    /// the next occupant to start resumes, so a handle issued before the
    /// swap still answers its own requester and no number is reissued.
    /// `None` until a guest first leaves, or once resumed.
    pub(crate) retired_replies: Option<PendingReplies>,
}

impl WasmTrampolineState {
    /// Release the **wasm guest**: run its `unwire` pre-shutdown hook, drop
    /// the `Component`, and sync the now-empty slot, so the accept set clears
    /// and only the framework cost cells stay. The caller ends the mailbox:
    /// a `DropComponent` closes the trampoline, and the host's module-boot
    /// `BootTeardown` vacates it.
    pub fn release_guest(&mut self, ctx: &mut NativeCtx<'_, WasmTrampoline>) {
        if let Some(mut component) = self.component.take() {
            // Issue 584 Phase 3 (ADR-0079 amended): unwire is the
            // single pre-shutdown hook — the legacy `on_drop`
            // retired alongside `WasmActor::on_drop`. Component
            // drops at end of scope, tearing down linear memory.
            component.unwire();
            // #6400: after `unwire`, which may still send.
            self.retired_correlations = Some(component.correlation_cursor());
            // #6409: after `unwire`, which may still answer handles.
            // ADR-0243 §6: held slots settle here. Releasing saves no guest
            // state, so no ticket survives to answer one, and its settlement
            // hold would keep the requester's chain open until actor close.
            let mut replies = component.take_pending_replies();
            replies.settle_held();
            self.retired_replies = Some(replies);
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
        // handler folds into its cell just after it returns, and a vacated
        // boot slot goes on dispatching its task wakes. The re-seed is
        // neutral, which is the honest reading of an estimate whose occupant
        // just changed.
        ctx.sync_guest(self);
    }
}
