use std::sync::Arc;

use aether_actor::Local as _;
use aether_substrate::actor::native::{Dispatch, NativeCtx};
use aether_substrate::actor::wasm::component::{Component, ComponentCtx};
use aether_substrate::actor::wasm::kind_manifest::ActorInputs;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use aether_substrate::mail::{CostCells, MailboxId};
use wasmtime::{Engine, Linker, Module};

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
pub struct WasmTrampolineState {
    /// `Some` while wasm is loaded; `None` after a `DropComponent`.
    /// Mail arriving in the `None` state warn-drops via the
    /// fallback (the trampoline is just an empty named slot).
    pub component: Option<Component>,
    /// Held for [`Self::handle_replace`] so a fresh
    /// `Component::instantiate` against the same engine + linker
    /// is reachable from the handler.
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<ComponentCtx>>,
    pub registry: Arc<Registry>,
    pub mailer: Arc<Mailer>,
    pub outbound: Arc<HubOutbound>,
    /// The trampoline's own mailbox id — the registry's depth-1
    /// derivation over `full_name`. Cached because
    /// `NativeCtx` only exposes `self_id()` via the
    /// `NativeInitCtx` flavour today; storing it here avoids
    /// reaching into `ctx.binding().self_mailbox()` on every
    /// handler call.
    pub mailbox: MailboxId,
    /// ADR-0096: the selected export's actor-type tag, or `None`
    /// for the entry type. Held so [`Self::handle_replace`]
    /// re-instantiates the same exported type from the new wasm
    /// and re-reads that type's capability group.
    pub type_tag: Option<u64>,
    /// ADR-0097: the resident `Module`, retained so a sibling spawn
    /// re-instantiates it (a cheap `Arc` clone — wasmtime shares the
    /// compiled code) without a re-compile, and refreshed on replace.
    pub module: Module,
    /// ADR-0097: every exported type's capability group (see
    /// [`super::WasmTrampolineConfig::actor_caps`]). A spawned sibling looks
    /// up its own handler set here by actor-type tag.
    pub actor_caps: Vec<ActorInputs>,
    /// ADR-0163 §3 (#3984): the resident module's raw wasm bytes, retained
    /// so a `spawn_child::<Sibling>` from this module can index its own
    /// asset load window, and refreshed on replace. Shared `Arc` — indexed,
    /// never mutated.
    pub wasm_bytes: Arc<[u8]>,
}

impl WasmTrampolineState {
    /// Unload the **wasm component**: run the guest's `unwire` pre-shutdown
    /// hook, drop the `Component`, clear the mailbox's accept-set, re-seed the
    /// trampoline's own framework cost cells, and vacate the mailbox. The
    /// trampoline itself stays alive as an empty slot a `ReplaceComponent`
    /// can refill. Both a `DropComponent` and the host's module-boot
    /// `BootTeardown` end here.
    pub fn unload(&mut self, ctx: &mut NativeCtx<'_>) {
        if let Some(mut component) = self.component.take() {
            // Issue 584 Phase 3 (ADR-0079 amended): unwire is the
            // single pre-shutdown hook — the legacy `on_drop`
            // retired alongside `WasmActor::on_drop`. Component
            // drops at end of scope, tearing down linear memory.
            component.unwire();
        }
        // iamacoffeepot/aether#1037: clear the trampoline's
        // capabilities — the wasm is unloaded, so the mailbox now
        // accepts nothing until a `replace` refills it. The
        // trampoline (and its mailbox name) survives as an empty
        // slot, but it has no accept-set while empty.
        self.mailer.capability_registry().remove(self.mailbox);
        // iamacoffeepot/aether#1128: drop the unloaded guest's per-handler
        // cost cells from the global table and the per-actor cache.
        // `unload` runs on the trampoline's own thread inside
        // `with_stamped`, so both indexes clear together.
        //
        // The trampoline's own framework arms are re-seeded rather than
        // dropped with them (iamacoffeepot/aether#4269): the mailbox survives
        // this as an empty refillable slot and goes on dispatching
        // `ReplaceComponent`, `DropComponent` and its task wakes, so retiring
        // their cells left the arms that outlive the guest unmeasured — the
        // unloading handler among them, which folds into its cell just after
        // it returns. The re-seed is neutral, which is the honest reading of
        // an estimate whose occupant just changed.
        self.mailer.cost_table().drop_mailbox(self.mailbox);
        let framework_kinds = <WasmTrampoline as Dispatch<Self>>::measured_kinds();
        let seeded = self.mailer.cost_table().seed(self.mailbox, &framework_kinds);
        CostCells::try_with_mut(|cells| cells.seed(seeded));
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
