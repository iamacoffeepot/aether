//! The wasm-trampoline runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`WasmTrampoline`] identity never names
//! these `aether_substrate` / `wasmtime`-typed types. The substrate-typed
//! imports are gated once by this module rather than line-by-line, beside the
//! `#[runtime] impl` that reaches the state, ctx, config, and republish
//! helpers.
//!
//! The cap is heavy and already decomposed, so unlike `aether.fs`'s
//! single-file `runtime.rs` the runtime half is a directory module:
//! [`state`] (the field-bearing `WasmTrampolineState` and its guest
//! [`Slot`]), [`config`] (the `WasmTrampolineConfig` init
//! bundle), [`republish`] (one member's prepare, commit and abort, ADR-0241
//! §7), [`aliases`] (inline-child alias staging), and [`contract`] (the
//! carried-context refusal and the inline-child types, ADR-0231 §4).

mod aliases;
mod config;
mod contract;
mod republish;
mod state;

pub use config::WasmTrampolineConfig;
pub use state::WasmTrampolineState;

// The `aether_substrate` / `wasmtime` / `std` names the parent `#[actor] impl`
// body references, re-exported so the parent `use runtime::*` glob sees them
// (the fs `runtime.rs` `pub use` pattern). `DropResult` rides this glob;
// `DropComponent` stays at the parent file root (always-on, for the
// `HandlesKind<K>` markers).
pub use std::io;
pub use std::sync::Arc;

use super::WasmTrampoline;
use crate::component::{Abort, Aborted, Commit, Committed, LoadDelivered, Prepare, Prepared, SpawnDelivered};
pub use aether_actor::Local;
use aether_actor::{Anyone, Single, runtime};
use aether_kinds::{ComponentCapabilities, SpawnResult};
pub use aether_kinds::{DropComponent, DropResult, LoadResult};
use aether_substrate::actor::native::ctx::GuestHost;
pub use aether_substrate::actor::native::envelope::Envelope;
pub use aether_substrate::actor::native::{
    Dispatch, NativeActor, NativeCtx, NativeInitCtx, Pending, RegistryBatchResult, TaskDone,
};
pub use aether_substrate::actor::wasm::asset_manifest;
pub use aether_substrate::actor::wasm::component::Component;
pub use aether_substrate::chassis::error::BootError;
#[allow(unused_imports, reason = "runtime facade retains its established KindId re-export")]
pub use aether_substrate::mail::{CostCell, CostCells, KindId};
use state::Slot;

/// The trampoline hosts a wasm guest, and its receive surface is that
/// guest's: the substrate reads it here through [`NativeCtx::sync_guest`].
impl GuestHost for WasmTrampoline {
    fn guest(state: &WasmTrampolineState) -> Option<&ComponentCapabilities> {
        // A prepared slot still hosts its kept guest: the candidate's surface
        // is registered only on commit.
        match state.slot {
            Slot::Live(_) | Slot::Prepared(_) => Some(&state.capabilities),
            Slot::Released => None,
        }
    }
}

#[runtime]
impl NativeActor for WasmTrampoline {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// field-bearing [`WasmTrampolineState`] holding the wasm `Component` and
    /// the substrate handles.
    type State = WasmTrampolineState;

    type Config = WasmTrampolineConfig;

    /// The trampoline's diagnostics label, consumed by `#[runtime]`, which
    /// validates it and drops it; the hand-written `Addressable` in
    /// `identity.rs` carries the same label. It names no
    /// address: a guest is born under its own published name (ADR-0241 §5).
    const NAMESPACE: &'static str = super::identity::TRAMPOLINE_LABEL;

    fn init(config: WasmTrampolineConfig, ctx: &mut NativeInitCtx<'_>) -> Result<WasmTrampolineState, BootError> {
        let mut substrate_ctx = ctx.guest_ctx(Arc::clone(&config.outbound));
        // ADR-0163 §3 (#3984): open an asset load window over the code this
        // instance's load brought and install it before instantiate, so the
        // guest's `init` (run inside `instantiate`) and its later `wire` can
        // pull assets through the `asset_fetch_p32` host fn. The window owns
        // the code, which the state never keeps, and lets go of it once
        // `wire` returns (below).
        substrate_ctx.install_load_window(asset_manifest::LoadWindow::open(&config.module, config.code));
        // ADR-0231 §4: an inline child the guest spawns publishes its own
        // namespace and rows, read from this module's exported and private
        // groups.
        substrate_ctx.install_inline_children(contract::inline_children(config.module.manifest()));
        // ADR-0090 (issue 1257): thread the load mail's config bytes
        // into the guest's typed `init`. An empty slice ("no config")
        // is decoded uniformly by a `Config = ()` guest via
        // `impl Kind for ()`; a typed-config guest decodes its
        // `Self::Config` from these bytes.
        let component = Component::instantiate(
            &config.engine,
            &config.linker,
            config.module.compiled(),
            substrate_ctx,
            &config.config,
            config.type_tag,
        )
        .map_err(|e| BootError::Other(io::Error::other(format!("wasm instantiation failed: {e}")).into()))?;

        // iamacoffeepot/aether#1128: seed this component's per-handler
        // cost cells from the guest's declared handler set
        // (`config.capabilities`, parsed from the wasm's
        // `aether.kinds.inputs` section). `init` runs inside the spawn
        // path's `with_stamped(&slots, …)`, so the per-actor
        // `CostCells` cache is stamped directly here — the cap's
        // thread vs the trampoline's is irrelevant: the stamp binds to
        // the actor's `ActorSlots`, not to a thread. Exact
        // `Arc<CostCell>`s stay actor-local until the fused owner
        // commit installs those same arcs globally.
        //
        // The trampoline's own framework arms are seeded with them, the same
        // set `NativeCtx::sync_guest` seeds (iamacoffeepot/aether#4269), so the
        // sync in `wire` finds every row already present and adds none. A row
        // it added there would be no birth's to roll back, and a birth
        // cancelled after `wire` would leave it behind to refuse the next
        // birth at this position.
        let mut measured = <WasmTrampoline as Dispatch<WasmTrampolineState>>::measured_kinds();
        let guest: Vec<KindId> =
            config.capabilities.handlers.iter().map(|h| h.id).filter(|id| !measured.contains(id)).collect();
        measured.extend(guest);
        let seeded = measured.into_iter().map(|kind| (kind, Arc::new(CostCell::new()))).collect();
        CostCells::try_with_mut(|cells| cells.seed(seeded));

        Ok(WasmTrampolineState {
            slot: Slot::Live(Box::new(component)),
            engine: config.engine,
            linker: config.linker,
            outbound: config.outbound,
            capabilities: config.capabilities,
            type_tag: config.type_tag,
            config: config.config,
            module: config.module,
            modules: config.modules,
            drops: Vec::new(),
        })
    }

    /// Register the guest's accept set and cost rows from this actor's
    /// [`GuestHost`] declaration, then fire the wasm guest's `wire` hook.
    /// Every birth of a trampoline — a load or a module boot — runs this
    /// hook, so the declaration is the accept set's one writer and the set
    /// is in place before the guest's `wire` sends anything.
    ///
    /// Issue 640 Phase 2: the guest's `wire` fires post-registration. The
    /// cap-side spawn flow registers the trampoline mailbox in step 5–7; this
    /// hook runs after that as part of the dispatcher's lifecycle, so a wire-time
    /// `aether.window.subscribe` mail proves against a live closure
    /// entry. Pre-issue-640 the call lived inside
    /// `Component::instantiate` (step 4, before registration) and
    /// races the window cap proving the subscriber at receipt through
    /// `ctx.resolve_live`, silently dropping subscribes.
    ///
    /// The guest's `wire` sends inherit this ctx's in-flight root, the
    /// birth's wire root (ADR-0244), so they settle with the rest of the
    /// birth's `wire` mail under one root a test can await.
    ///
    /// A guest whose `wire` faults fails this birth (ADR-0247 rule 3), and
    /// the load or spawn that asked for it answers `Err` with the fault. A
    /// guest that returned an error is intact and stays in its slot, so the
    /// close that follows runs its `unwire`. A guest that trapped is
    /// released here, so the close finds no guest and runs no more of its
    /// code, as a guest whose `init` trapped is dropped without a further
    /// call.
    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        ctx.sync_guest(state);
        let root = ctx.in_flight_root();
        let Slot::Live(component) = &mut state.slot else {
            return Ok(());
        };
        let Err(fault) = WasmTrampolineState::wire_guest(ctx, component, root) else {
            return Ok(());
        };

        let trapped = fault.is_trap();
        if trapped {
            state.slot = Slot::Released;
        }
        let outcome = if trapped {
            "trapped"
        } else {
            "failed"
        };
        Err(BootError::Other(io::Error::other(format!("wasm guest wire {outcome}: {fault}")).into()))
    }

    /// The close hook: release the guest (ADR-0241 §8, ADR-0247 rule 5). A
    /// live guest runs its own `unwire` export, has each reply it still
    /// holds answered with the `unanswered` value it registered (ADR-0243
    /// §6), and is dropped. Every close of the trampoline runs this: a
    /// `DropComponent`, an engine teardown, and a birth cancelled after
    /// `wire`. Engine teardown answers nothing for the guest, and a prepared
    /// slot unwires a candidate iff it wired and unwires the kept guest. A
    /// drop request the close was asked for is answered here, once the guest
    /// is released (see `WasmTrampolineState::close_guest`).
    fn unwire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        state.close_guest(ctx);
    }

    /// Ask this instance to close (ADR-0241 §8). The request shuts the
    /// trampoline down and holds its answer; the close that follows the
    /// handler drains the mail still queued to the guest, releases the guest
    /// through its `unwire` hook, which answers this request `Ok`, and then
    /// tombstones the name, retires its route to `Dropped`, and sends each
    /// watcher of the mailbox and of its inline-child aliases a
    /// `MonitorNotice`. A later load of the name is refused as retired, and
    /// nothing refills it.
    ///
    /// `DropResult::Ok` therefore means what it always has: the guest ran
    /// its `unwire`, its held replies are answered, and its mailbox accepts
    /// nothing. The guest's `unwire` runs in the close, with no mail in
    /// flight, so what it sends starts its own chains; those sends are on
    /// their recipients' inboxes before this request is answered.
    #[handler::request]
    fn on_drop_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _payload: DropComponent,
    ) -> Pending<DropResult> {
        let (pending, held) = ctx.hold::<DropResult>();
        state.drops.push(held);
        ctx.shutdown();
        pending
    }

    /// Answer the requester of the load that produced this trampoline, in the
    /// trampoline's own name (ADR-0230 §3).
    ///
    /// The component host hands its held load reply here once this
    /// guest's birth completes (`Held::hand_off`), so the mail's
    /// reply target is the original requester and the reply's stamped sender
    /// is this trampoline — the reference the requester keeps.
    ///
    /// The hand-off pins the reply target, and the reply target is the mail's
    /// sender, so the handler cannot tell a host hand-off from any other
    /// delivery by its sender. It needs no such check: it answers only the
    /// mail's own reply target, so a delivery from anyone else reaches only
    /// the actor that sent it.
    #[handler::request]
    fn on_load_delivered(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, payload: LoadDelivered) -> LoadResult {
        let LoadDelivered { path, capabilities } = payload;
        LoadResult::Ok { path, capabilities }
    }

    /// Answer the requester of a spawn that names this trampoline, in the
    /// trampoline's own name (ADR-0230 §3, ADR-0241 §9): `Live` when it was
    /// already live and nothing was re-initialised, `Spawned` when the spawn
    /// just stood it up. The host hands its held spawn reply here the way it
    /// hands a load's to [`Self::on_load_delivered`], which answers only the
    /// mail's own reply target in the same way.
    #[handler::request]
    fn on_spawn_delivered(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, payload: SpawnDelivered) -> SpawnResult {
        let SpawnDelivered { path, capabilities, live } = payload;
        if live {
            SpawnResult::Live { path, capabilities }
        } else {
            SpawnResult::Spawned { path, capabilities }
        }
    }

    /// Prepare a candidate of this guest's type from `code` beside the
    /// running guest (ADR-0241 §7). Until a commit or an abort, mail for the
    /// guest waits at its inbox gate and nothing the candidate sends leaves.
    /// A refusal leaves the running guest in place, still wired, or closes
    /// the instance when the guest refuses the state it saved (ADR-0249 §4).
    #[handler::request]
    fn on_prepare(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Prepare) -> Prepared {
        let Prepare { code, config } = payload;
        let candidate =
            state.modules.check_in(&ctx.blob_check_in(), &code).and_then(|module| state.candidate_type(module));
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(error) => return Prepared::Refused { error },
        };

        let target = ctx.path();
        state.prepare(ctx, &target, candidate, code, config)
    }

    /// Install the prepared candidate (ADR-0241 §7): the old guest unwires
    /// first, then its held mail leaves on this commit's chain, and the mail
    /// the gate queued is delivered to the candidate in order. A commit with
    /// nothing prepared is a host bug and aborts the substrate.
    #[handler::request]
    fn on_commit(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _payload: Commit) -> Committed {
        state.commit(ctx);
        Committed
    }

    /// Discard the prepared candidate and its held mail, unwiring it exactly
    /// when it wired, and reinstate the running guest, still wired, with the
    /// mail the gate queued (ADR-0241 §7), or close the instance when the
    /// guest refuses the state it saved (ADR-0249 §4). With nothing prepared
    /// it answers at once.
    #[handler::request]
    fn on_abort(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _payload: Abort) -> Aborted {
        state.abort(ctx);
        Aborted
    }

    #[handler(task)]
    fn on_inline_alias_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<RegistryBatchResult>) {
        state.finish_inline_aliases(ctx, done);
    }

    /// Forward un-handled mail to the wasm guest.
    ///
    /// The framework dispatcher pulled this envelope from the
    /// trampoline's binding, dispatched against typed handlers
    /// (none matched), and called this fallback. The envelope goes
    /// to `Component::deliver` as routed, and the guest's
    /// `receive_p32` dispatch shim does the rest.
    ///
    /// While a republish is prepared the inbox gate is closed (ADR-0241
    /// §7): the mail is taken off the dispatcher, so its chain stays open,
    /// and waits in order for the commit or abort to deliver it to the guest
    /// that wins. The trampoline's typed rows are never gated.
    #[fallback]
    fn forward_to_wasm(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Anyone, Single>, env: &Envelope) -> bool {
        match &mut state.slot {
            Slot::Live(component) => WasmTrampolineState::deliver_to_guest(ctx, component, env),
            Slot::Prepared(prepared) => prepared.gated.push_back(ctx.take_inbound()),
            // The slot is empty inside the close, after the residual drain,
            // and from a reinstatement whose guest refused its own state
            // until the close that reinstatement asked for (ADR-0249 §4).
            // Mail drained to it in between is discarded here.
            Slot::Released => tracing::warn!(
                target: "aether_component",
                actor = %ctx.path(),
                kind = %ctx.kind_label(env.kind),
                "mail to trampoline with no wasm loaded (guest released); discarded",
            ),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use aether_actor::Contracts;

    use super::*;

    /// Drift guard: the hand-written contract rows in `identity.rs` must match
    /// the handler rows the `#[runtime]` impl dispatches, row for row and in
    /// order. A handler added to the runtime impl without its hand-written row
    /// would otherwise surface only as a refused typed send or hand-off.
    #[test]
    fn the_hand_written_contracts_match_the_dispatch_table() {
        let dispatched: Vec<(KindId, aether_data::ReplyContract)> =
            <WasmTrampoline as Dispatch<WasmTrampolineState>>::capabilities()
                .handlers
                .iter()
                .map(|handler| (handler.id, handler.reply))
                .collect();

        assert_eq!(<WasmTrampoline as Contracts>::CONTRACTS, dispatched.as_slice());
    }
}
