//! The wasm-trampoline runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`WasmTrampoline`] identity never names
//! these `aether_substrate` / `wasmtime`-typed types. The substrate-typed
//! imports are gated once by this module rather than line-by-line; the
//! `#[actor] impl` in the parent reaches the state, ctx, config, and replace
//! helpers through the single `use runtime::*` glob.
//!
//! The cap is heavy and already decomposed, so unlike `aether.fs`'s
//! single-file `runtime.rs` the runtime half is a directory module:
//! [`state`] (the field-bearing `WasmTrampolineState`), [`config`] (the
//! `WasmTrampolineConfig` init bundle), [`replace`] (the inherent replace
//! impl on the state), and [`contract`] (the replace-time
//! contract refusal, ADR-0231 §5).

mod config;
mod contract;
mod replace;
mod state;

pub use config::WasmTrampolineConfig;
pub use state::WasmTrampolineState;

// The `aether_substrate` / `wasmtime` / `std` names the parent `#[actor] impl`
// body references, re-exported so the parent `use runtime::*` glob sees them
// (the fs `runtime.rs` `pub use` pattern). `DropResult` / `ReplaceResult` ride
// this glob; `DropComponent` / `ReplaceComponent` stay at the parent file root
// (always-on, for the `HandlesKind<K>` markers).
pub use std::io;
pub use std::sync::Arc;

use super::WasmTrampoline;
use crate::component::LoadDelivered;
use crate::kinds::BootTeardown;
pub use aether_actor::Local;
use aether_actor::{Single, runtime};
use aether_kinds::ComponentCapabilities;
pub use aether_kinds::{DropComponent, DropResult, LoadResult, ReplaceComponent, ReplaceResult};
use aether_substrate::actor::native::ctx::GuestHost;
pub use aether_substrate::actor::native::envelope::Envelope;
pub use aether_substrate::actor::native::{
    Dispatch, NativeActor, NativeCtx, NativeInitCtx, RegistryBatchResult, TaskDone,
};
pub use aether_substrate::actor::wasm::asset_manifest;
pub use aether_substrate::actor::wasm::component::Component;
pub use aether_substrate::chassis::error::BootError;
#[allow(unused_imports, reason = "runtime facade retains its established KindId re-export")]
pub use aether_substrate::mail::{CostCell, CostCells, KindId};

/// The trampoline hosts a wasm guest, and its receive surface is that
/// guest's: the substrate reads it here through [`NativeCtx::sync_guest`].
impl GuestHost for WasmTrampoline {
    fn guest(state: &WasmTrampolineState) -> Option<&ComponentCapabilities> {
        state.component.as_ref().map(|_| &state.capabilities)
    }
}

#[runtime]
impl NativeActor for WasmTrampoline {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// field-bearing [`WasmTrampolineState`] holding the wasm `Component` and
    /// the substrate handles.
    type State = WasmTrampolineState;

    type Config = WasmTrampolineConfig;

    /// The trampoline's own namespace, **forward-fed** from
    /// [`EMBEDDED_SCOPE`] — `aether-actor`'s sole owner of the
    /// `"aether.embedded"` literal — until #6869 retires it. No guest is
    /// named by it: a guest is born under its own published name (ADR-0241
    /// §5), so its id depends on what the code is, not how it is hosted.
    const NAMESPACE: &'static str = EMBEDDED_SCOPE;

    fn init(config: WasmTrampolineConfig, ctx: &mut NativeInitCtx<'_>) -> Result<WasmTrampolineState, BootError> {
        let mut substrate_ctx = ctx.guest_ctx(Arc::clone(&config.outbound));
        // ADR-0163 §3 (#3984): open an asset load window over the module's
        // asset blobs and install it before instantiate, so the guest's
        // `init` (run inside `instantiate`) and its later `wire` can pull
        // assets through the `asset_fetch_p32` host fn. Closed once `wire`
        // returns (below).
        substrate_ctx.install_load_window(asset_manifest::LoadWindow::open(&config.module));
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
            component: Some(component),
            engine: config.engine,
            linker: config.linker,
            outbound: config.outbound,
            capabilities: config.capabilities,
            type_tag: config.type_tag,
            module: config.module,
            modules: config.modules,
            retired_correlations: None,
            retired_replies: None,
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
    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        ctx.sync_guest(state);
        let (aliases, retired) = state.component.as_mut().map_or_else(Default::default, |component| {
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
            (component.drain_pending_aliases(), component.drain_pending_alias_retirements())
        });
        WasmTrampolineState::stage_inline_aliases(ctx, aliases);
        WasmTrampolineState::stage_inline_alias_retirements(ctx, retired);
    }

    /// Close this instance (ADR-0241 §8). Releases the guest, which runs its
    /// `unwire` pre-shutdown hook and drops the `Component`, then shuts the
    /// trampoline down. The close tail tombstones the name, retires its route
    /// to `Dropped`, and sends each watcher of the mailbox and of its
    /// inline-child aliases a `MonitorNotice`. A later load of the name is
    /// refused as retired, and nothing refills it.
    #[handler::single]
    fn on_drop_component(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _payload: DropComponent) -> DropResult {
        state.release_guest(ctx);
        ctx.shutdown();
        DropResult::Ok
    }

    /// The component host's module-boot teardown (ADR-0147): the host sends
    /// it when the module's last non-boot actor departs. The trampoline
    /// releases its guest and vacates its mailbox, with no reply, and stays
    /// an empty slot until the substrate stops.
    #[handler::single]
    fn on_boot_teardown(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _payload: BootTeardown) {
        state.release_guest(ctx);
        // ADR-0079 §8 (amended, issue 3741): drain this trampoline's
        // watchers and fire one `MonitorNotice` each, so every cap holding
        // state keyed by this mailbox purges its own rows.
        ctx.vacate();
    }

    /// Replace the wasm component with a fresh module. ADR-0022 +
    /// ADR-0038 splice invariants hold because the trampoline's
    /// inbox is the framework binding, which outlives the
    /// `Component` swap. `on_dehydrate` runs on the old instance,
    /// `take_saved_state` lifts any rehydration bundle, the new
    /// module instantiates against the same binding, and
    /// `on_rehydrate` runs on the fresh side.
    #[handler::single]
    fn on_replace_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: ReplaceComponent,
    ) -> ReplaceResult {
        state.handle_replace(ctx, payload)
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
    #[handler::single]
    fn on_load_delivered(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, payload: LoadDelivered) -> LoadResult {
        let LoadDelivered { path, capabilities } = payload;
        LoadResult::Ok { path, capabilities }
    }

    #[handler(task)]
    fn on_inline_alias_done(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<RegistryBatchResult>) {
        WasmTrampolineState::finish_inline_aliases(ctx, done);
    }

    /// Forward un-handled mail to the wasm guest.
    ///
    /// The framework dispatcher pulled this envelope from the
    /// trampoline's binding, dispatched against typed handlers
    /// (none matched), and called this fallback. The envelope goes
    /// to `Component::deliver` as routed, and the guest's
    /// `receive_p32` dispatch shim does the rest.
    #[fallback]
    fn forward_to_wasm(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Single>, env: &Envelope) -> bool {
        // Deliver the inbound, then drain the inline-child aliases and
        // retirements the guest staged during `deliver`. The block scopes
        // the `&mut component` borrow to the guest call.
        let (aliases, retired) = {
            let Some(component) = state.component.as_mut() else {
                tracing::warn!(
                    target: "aether_component",
                    actor = %ctx.path(),
                    kind = %ctx.kind_label(env.kind),
                    "mail to trampoline with no wasm loaded (guest released); discarded",
                );
                return true;
            };
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
            (component.drain_pending_aliases(), component.drain_pending_alias_retirements())
        };
        WasmTrampolineState::stage_inline_aliases(ctx, aliases);
        WasmTrampolineState::stage_inline_alias_retirements(ctx, retired);
        true
    }
}
