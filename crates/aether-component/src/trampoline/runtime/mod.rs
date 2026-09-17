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
//! `WasmTrampolineConfig` init bundle), and [`replace`] (the inherent replace /
//! sibling-spawn impl on the state).

mod config;
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
use crate::ComponentRestrictions;
pub use aether_actor::Local;
use aether_actor::{Single, runtime};
pub use aether_kinds::{DropComponent, DropResult, ReplaceComponent, ReplaceResult};
pub use aether_substrate::actor::native::envelope::Envelope;
pub use aether_substrate::actor::native::{
    Dispatch, NativeActor, NativeCtx, NativeInitCtx, RegistryBatchResult, SpawnOutcome, TaskDone,
};
pub use aether_substrate::actor::wasm::asset_manifest;
pub use aether_substrate::actor::wasm::component::{Component, ComponentCtx};
pub use aether_substrate::chassis::error::BootError;
#[allow(unused_imports, reason = "runtime facade retains its established KindId re-export")]
pub use aether_substrate::mail::{CostCell, CostCells, KindId, Mail};

#[runtime]
impl NativeActor for WasmTrampoline {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// field-bearing [`WasmTrampolineState`] holding the wasm `Component` and
    /// the substrate handles.
    type State = WasmTrampolineState;

    type Config = WasmTrampolineConfig;

    /// The embedding-host scope namespace (ADR-0099 §5/§6, ADR-0119),
    /// **forward-fed** from [`EMBEDDED_SCOPE`]
    /// — `aether-actor`'s sole owner of the `"aether.embedded"` literal.
    /// The trampoline references the const rather than re-declaring the
    /// name, so an embeddable actor's id depends on what the code is, not
    /// how it is hosted, and the namespace is written only on its owner.
    /// Reachable on every target because `#[actor]` emits the always-on
    /// `Addressable` impl. ADR-0097: the substrate's `TRAMPOLINE_NAMESPACE`
    /// forward-feeds the same const, collapsing the former two-literal mirror
    /// into one source; the `trampoline_namespace_matches_substrate` test
    /// guards the match.
    const NAMESPACE: &'static str = EMBEDDED_SCOPE;

    fn init(config: WasmTrampolineConfig, ctx: &mut NativeInitCtx<'_>) -> Result<WasmTrampolineState, BootError> {
        let mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let mut substrate_ctx =
            ComponentCtx::new(mailbox, Arc::clone(&config.registry), Arc::clone(&mailer), Arc::clone(&config.outbound));
        // Wire the trampoline's binding so the guest's reply /
        // outbound-mail host fns route through *this* trampoline's
        // binding (issue 634 Phase 4 PR 3 — single source of inbox
        // truth lives on `NativeBinding`, not on `ComponentCtx`).
        substrate_ctx.install_binding(Arc::clone(ctx.binding()));
        // ADR-0163 §3 (#3984): index an asset load window over the module's
        // `aether.asset.*` sections and install it before instantiate, so
        // the guest's `init` (run inside `instantiate`) and its later `wire`
        // can pull assets through the `asset_fetch_p32` host fn. Closed once
        // `wire` returns (below). The bytes are validated at load, so an
        // index error here is a torn build rather than a user error.
        let load_window = asset_manifest::LoadWindow::index(Arc::clone(&config.wasm_bytes))
            .map_err(|e| BootError::Other(io::Error::other(format!("asset index failed: {e}")).into()))?;
        substrate_ctx.install_load_window(load_window);
        // ADR-0090 (issue 1257): thread the load mail's config bytes
        // into the guest's typed `init`. An empty slice ("no config")
        // is decoded uniformly by a `Config = ()` guest via
        // `impl Kind for ()`; a typed-config guest decodes its
        // `Self::Config` from these bytes.
        let component = Component::instantiate(
            &config.engine,
            &config.linker,
            &config.module,
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
        // commit installs those same arcs globally. Replace continues to
        // re-seed on the live trampoline's own dispatch; drop clears both
        // indexes.
        let seeded =
            config.capabilities.handlers.iter().map(|handler| (handler.id, Arc::new(CostCell::new()))).collect();
        CostCells::try_with_mut(|cells| cells.seed(seeded));

        Ok(WasmTrampolineState {
            prohibit: config.prohibit,
            component: Some(component),
            engine: config.engine,
            linker: config.linker,
            registry: config.registry,
            mailer,
            outbound: config.outbound,
            mailbox,
            type_tag: config.type_tag,
            module: config.module,
            actor_caps: config.actor_caps,
            wasm_bytes: config.wasm_bytes,
        })
    }

    /// Issue 640 Phase 2: fire the wasm guest's `wire` hook
    /// post-registration. The cap-side spawn flow registers the
    /// trampoline mailbox in step 5–7; this hook runs after that
    /// as part of the dispatcher's lifecycle, so a wire-time
    /// `aether.window.subscribe` mail validates against a live closure
    /// entry. Pre-issue-640 the call lived inside
    /// `Component::instantiate` (step 4, before registration) and
    /// races the window cap's `validate_subscriber_mailbox`,
    /// silently dropping subscribes.
    fn wire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        let (aliases, retired) = state.component.as_mut().map_or_else(Default::default, |component| {
            if let Err(e) = component.wire() {
                tracing::error!(
                    target: "aether_component",
                    error = %e,
                    "wasm guest `wire` hook returned error",
                );
            }
            // ADR-0163 §3 (#3984): the asset load window closes when `wire`
            // returns — drop the payload pin and byte ranges so
            // `asset_fetch_p32` traps thereafter, retaining the catalog
            // metadata for the instance's life. Runs whether or not `wire`
            // errored; the window's job (init + wire) is done either way.
            component.close_load_window();
            (component.drain_pending_aliases(), component.drain_pending_alias_retirements())
        });
        state.stage_inline_aliases(ctx, aliases);
        state.stage_inline_alias_retirements(ctx, retired);
    }

    /// Application teardown is a different lifecycle path from an
    /// individual `DropComponent` request. The bootstrap prohibition does
    /// not prevent the resident guest from unwiring during actor shutdown.
    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if let Some(component) = state.component.as_mut() {
            component.unwire();
        }
    }

    /// Drop the **wasm component**. Runs the guest's `unwire`
    /// pre-shutdown hook, then drops the `Component`. The trampoline itself
    /// stays alive — the mailbox `aether.embedded:NAME`
    /// remains addressable and reusable: agents can refill it via
    /// `ReplaceComponent` without minting a new name. To kill
    /// the trampoline (tombstone the subname), terminate the
    /// substrate.
    ///
    /// Mail arriving in the dropped state falls through to
    /// [`Self::forward_to_wasm`], which warn-drops because
    /// `state.component` is `None`.
    #[handler::single]
    fn on_drop_component(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _payload: DropComponent) -> DropResult {
        if state.prohibit.contains(ComponentRestrictions::DROP) {
            return DropResult::Err { error: "component drop prohibited by native bootstrap".to_owned() };
        }

        if let Some(mut component) = state.component.take() {
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
        state.mailer.capability_registry().remove(state.mailbox);
        // iamacoffeepot/aether#1128: drop the unloaded guest's per-handler
        // cost cells from the global table and the per-actor cache.
        // `on_drop_component` runs on the trampoline's own thread
        // inside `with_stamped`, so both indexes clear together.
        //
        // The trampoline's own framework arms are re-seeded rather than
        // dropped with them (iamacoffeepot/aether#4269): the mailbox survives
        // this as an empty refillable slot and goes on dispatching
        // `ReplaceComponent`, `DropComponent` and its task wakes, so retiring
        // their cells left the arms that outlive the guest unmeasured — this
        // very handler among them, which folds into its cell just after it
        // returns. The re-seed is neutral, which is the honest reading of an
        // estimate whose occupant just changed.
        state.mailer.cost_table().drop_mailbox(state.mailbox);
        let framework_kinds = <Self as Dispatch<WasmTrampolineState>>::measured_kinds();
        let seeded = state.mailer.cost_table().seed(state.mailbox, &framework_kinds);
        CostCells::try_with_mut(|cells| cells.seed(seeded));
        // ADR-0079 §8 (amended, issue 3741): declare the mailbox
        // vacated — drain this trampoline's watchers and fire one
        // `MonitorNotice` each, so every cap holding state keyed by
        // this mailbox (input subscriptions, lifecycle stages, http
        // routes) purges its own rows. The slot stays live for a
        // `replace` refill; the next occupant's watchers register
        // fresh.
        ctx.vacate();
        DropResult::Ok
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

    #[handler(task)]
    fn on_sibling_spawn_done(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome, replace::SiblingSpawnContext>,
    ) {
        state.finish_sibling_spawn(done);
    }

    #[handler(task)]
    fn on_inline_alias_done(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        done: TaskDone<RegistryBatchResult, replace::InlineAliasContext>,
    ) {
        WasmTrampolineState::finish_inline_aliases(done);
    }

    /// Forward un-handled mail to the wasm guest.
    ///
    /// The framework dispatcher pulled this envelope from the
    /// trampoline's binding, dispatched against typed handlers
    /// (none matched), and called this fallback. We synthesise a
    /// `Mail` with the trampoline's own id as recipient, hand it
    /// to `Component::deliver`, and let the guest's `receive_p32`
    /// dispatch shim do the rest.
    #[fallback]
    fn forward_to_wasm(state: &mut Self::State, ctx: &mut NativeCtx<'_, Single, Self>, env: &Envelope) -> bool {
        // ADR-0097: deliver the inbound, then drain every sibling spawn
        // the guest staged during `deliver`. The block scopes the
        // `&mut component` borrow so `spawn_sibling` can read the
        // trampoline's other fields afterward.
        let (aliases, retired, pendings) = {
            let Some(component) = state.component.as_mut() else {
                tracing::warn!(
                    target: "aether_component",
                    mailbox = %state.mailbox,
                    kind = %ctx.mailer().registry().kind_label(env.kind),
                    "mail to trampoline with no wasm loaded (post-drop); discarded — re-load via aether.component.replace",
                );
                return true;
            };
            // Issue iamacoffeepot/aether#722: carry the inbound's
            // lineage through to the synthetic `Mail`.
            // `Component::deliver` reads `mail.mail_id` and `mail.root`
            // to populate `ComponentCtx`'s in-flight cells, so any
            // guest-triggered `send_mail_p32` / `reply_mail_p32` stamps
            // `parent_mail = Some(env.mail_id)` and inherits the chain
            // `root`. Without this, the trampoline's wrapped Mail
            // defaults to `MailId::NONE` and the guest's outbound looks
            // like a fresh root.
            // ADR-0114 §2: deliver the *routed* recipient as the guest
            // `Mail`'s recipient, not the trampoline's own id. For a
            // normally-addressed actor `env.recipient` equals
            // `state.mailbox`, so this is a no-op; for an inline-child
            // alias it carries the child's address, which
            // `Component::deliver` threads to the guest's `receive`
            // frame + the `ComponentCtx` dispatch identity so the
            // membrane demuxes to the child and the child's sends stamp
            // its address as origin.
            let mail = Mail::new(env.recipient, env.kind, env.payload.bytes().to_vec(), env.count)
                .with_reply_to(env.sender)
                .with_lineage(env.mail_id, env.root, env.parent_mail);
            if let Err(e) = component.deliver(&mail) {
                // ADR-0063 fail-fast: a wasm trap (or host-fn error
                // returned through `Component::deliver`) kills the
                // substrate. Wedge detection (CPU-loop guests) waits
                // on a future epoch-deadline ADR — symmetric with
                // native actors, which have no wedge guard either
                // today.
                let kind = ctx.mailer().registry().kind_label(env.kind);
                ctx.fatal_abort(format!("component {} (kind {kind}) trapped: {e}", state.mailbox));
            }
            (
                component.drain_pending_aliases(),
                component.drain_pending_alias_retirements(),
                component.drain_pending_spawns(),
            )
        };
        state.stage_inline_aliases(ctx, aliases);
        state.stage_inline_alias_retirements(ctx, retired);
        for pending in pendings {
            state.spawn_sibling(ctx, pending);
        }
        true
    }
}

#[cfg(test)]
mod lifecycle_restriction_tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aether_data::{Kind, MailId, MailboxId, Source};
    use aether_harness_substrate::test_helpers::require_wasm;
    use aether_substrate::actor::native::NativeBinding;
    use aether_substrate::actor::wasm::host_fns;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::{OwnedDispatch, Registry};
    use aether_substrate::testing::boot_authority;
    use aether_test_fixtures_kinds::{BootTornDown, SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME};
    use wasmtime::{Engine, Linker, Module};

    use super::*;

    fn state(
        wasm: &[u8],
        prohibit: ComponentRestrictions,
        type_tag: Option<u64>,
    ) -> (WasmTrampolineState, Arc<NativeBinding>) {
        let engine = Arc::new(Engine::default());
        let mut linker = Linker::new(&engine);
        host_fns::register(&mut linker).expect("register host functions");
        let linker = Arc::new(linker);
        let module = Module::new(&engine, wasm).expect("compile fixture");
        let registry = Arc::new(Registry::new());
        let outbound = HubOutbound::disconnected();
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound)));
        let mailbox = MailboxId(0x6135);
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), mailbox));
        let mut guest_ctx =
            ComponentCtx::new(mailbox, Arc::clone(&registry), Arc::clone(&mailer), Arc::clone(&outbound));
        guest_ctx.install_binding(Arc::clone(&binding));
        guest_ctx
            .install_load_window(asset_manifest::LoadWindow::index(Arc::from(wasm)).expect("index fixture assets"));
        let component =
            Component::instantiate(&engine, &linker, &module, guest_ctx, &[], type_tag).expect("instantiate fixture");

        (
            WasmTrampolineState {
                prohibit,
                component: Some(component),
                engine,
                linker,
                registry,
                mailer,
                outbound,
                mailbox,
                type_tag,
                module,
                actor_caps: Vec::new(),
                wasm_bytes: Arc::from(wasm),
            },
            binding,
        )
    }

    fn replace(wasm: &[u8], mailbox_id: MailboxId) -> ReplaceComponent {
        ReplaceComponent { mailbox_id, wasm: wasm.to_vec(), drain_timeout_ms: None, config: Vec::new(), export: None }
    }

    #[test]
    fn native_drop_prohibition_survives_successful_and_failed_replacement() {
        let Some(path) = require_wasm("aether_test_fixtures_bundle") else {
            return;
        };
        let wasm = fs::read(path).expect("read fixture");
        let (mut state, binding) = state(&wasm, ComponentRestrictions::DROP, None);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let mailbox = state.mailbox;

        assert!(matches!(
            WasmTrampoline::on_drop_component(&mut state, &mut ctx, DropComponent { mailbox_id: mailbox }),
            DropResult::Err { .. }
        ));
        assert!(state.component.is_some(), "rejected drop leaves the guest serving");
        assert!(matches!(state.handle_replace(&mut ctx, replace(b"invalid wasm", mailbox)), ReplaceResult::Err { .. }));
        assert!(state.component.is_some(), "failed replace leaves the guest serving");
        assert!(matches!(state.handle_replace(&mut ctx, replace(&wasm, mailbox)), ReplaceResult::Ok { .. }));
        assert_eq!(state.prohibit, ComponentRestrictions::DROP);
        assert!(matches!(
            WasmTrampoline::on_drop_component(&mut state, &mut ctx, DropComponent { mailbox_id: mailbox }),
            DropResult::Err { .. }
        ));
        assert!(state.component.is_some(), "successful replacement retains drop prohibition");

        assert!(state.component.is_some());
    }

    #[test]
    fn replace_prohibition_precedes_candidate_work_and_does_not_affect_sibling() {
        let Some(path) = require_wasm("aether_test_fixtures_bundle") else {
            return;
        };
        let wasm = fs::read(path).expect("read fixture");
        let (mut protected, binding) = state(&wasm, ComponentRestrictions::REPLACE | ComponentRestrictions::DROP, None);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let mailbox = protected.mailbox;

        let ReplaceResult::Err { error } = protected.handle_replace(&mut ctx, replace(b"invalid wasm", mailbox)) else {
            panic!("replacement must be prohibited");
        };
        assert!(error.contains("prohibited"), "the policy rejects before invalid wasm compilation: {error}");
        assert!(protected.component.is_some());
        assert!(matches!(
            WasmTrampoline::on_drop_component(&mut protected, &mut ctx, DropComponent { mailbox_id: mailbox }),
            DropResult::Err { .. }
        ));

        let (mut replace_only, replace_only_binding) = state(&wasm, ComponentRestrictions::REPLACE, None);
        let mut replace_only_ctx = NativeCtx::new(&replace_only_binding, Source::NONE, MailId::NONE, MailId::NONE);
        let replace_only_mailbox = replace_only.mailbox;
        assert!(matches!(
            replace_only.handle_replace(&mut replace_only_ctx, replace(&wasm, replace_only_mailbox)),
            ReplaceResult::Err { .. }
        ));
        assert!(matches!(
            WasmTrampoline::on_drop_component(
                &mut replace_only,
                &mut replace_only_ctx,
                DropComponent { mailbox_id: replace_only_mailbox }
            ),
            DropResult::Ok
        ));

        let (mut sibling, sibling_binding) = state(&wasm, ComponentRestrictions::NONE, None);
        let mut sibling_ctx = NativeCtx::new(&sibling_binding, Source::NONE, MailId::NONE, MailId::NONE);
        let sibling_mailbox = sibling.mailbox;
        assert!(matches!(
            WasmTrampoline::on_drop_component(
                &mut sibling,
                &mut sibling_ctx,
                DropComponent { mailbox_id: sibling_mailbox }
            ),
            DropResult::Ok
        ));
        assert!(sibling.component.is_none(), "a separately bootstrapped slot has its own policy");
        assert!(protected.component.is_some(), "sibling operation does not affect protected slot");
    }

    fn observe_boot_unwire(state: &WasmTrampolineState) -> Arc<AtomicUsize> {
        let observed = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&observed);
        state.registry.register_inbox(
            &boot_authority(),
            SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME,
            Arc::new(move |dispatch: OwnedDispatch| {
                if dispatch.kind == BootTornDown::ID {
                    captured.fetch_add(1, Ordering::SeqCst);
                }
                dispatch.discharge();
            }),
        );
        observed
    }

    #[test]
    #[allow(clippy::disallowed_methods, reason = "fixture export tag is derived from its declared namespace")]
    fn protected_application_shutdown_unwires_once_and_drop_does_not_double_unwire() {
        let Some(path) = require_wasm("aether_test_fixtures_boot") else {
            return;
        };
        let wasm = fs::read(path).expect("read boot fixture");
        let boot_tag = Some(aether_data::mailbox_id_from_name("aether.test.boot.boot").0);

        let (mut protected, protected_binding) = state(&wasm, ComponentRestrictions::DROP, boot_tag);
        let protected_events = observe_boot_unwire(&protected);
        let mut protected_ctx = NativeCtx::new(&protected_binding, Source::NONE, MailId::NONE, MailId::NONE);
        let protected_mailbox = protected.mailbox;
        assert!(matches!(
            WasmTrampoline::on_drop_component(
                &mut protected,
                &mut protected_ctx,
                DropComponent { mailbox_id: protected_mailbox }
            ),
            DropResult::Err { .. }
        ));
        assert_eq!(protected_events.load(Ordering::SeqCst), 0, "rejected individual drop does not unwire");
        <WasmTrampoline as aether_actor::Lifecycle<WasmTrampolineState>>::unwire(&mut protected, &mut protected_ctx);
        drop(protected_ctx);
        assert_eq!(protected_events.load(Ordering::SeqCst), 1, "application shutdown unwires a protected guest once");

        let (mut ordinary, ordinary_binding) = state(&wasm, ComponentRestrictions::NONE, boot_tag);
        let ordinary_events = observe_boot_unwire(&ordinary);
        let mut ordinary_ctx = NativeCtx::new(&ordinary_binding, Source::NONE, MailId::NONE, MailId::NONE);
        let ordinary_mailbox = ordinary.mailbox;
        assert!(matches!(
            WasmTrampoline::on_drop_component(
                &mut ordinary,
                &mut ordinary_ctx,
                DropComponent { mailbox_id: ordinary_mailbox }
            ),
            DropResult::Ok
        ));
        <WasmTrampoline as aether_actor::Lifecycle<WasmTrampolineState>>::unwire(&mut ordinary, &mut ordinary_ctx);
        drop(ordinary_ctx);
        assert_eq!(
            ordinary_events.load(Ordering::SeqCst),
            1,
            "shutdown does not repeat a prior individual drop's hook"
        );
    }
}
