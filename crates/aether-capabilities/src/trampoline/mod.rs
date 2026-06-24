//! `WasmTrampoline` — the `NativeActor` every loaded wasm component runs
//! as. Each loaded wasm component is one trampoline instance addressed at
//! `aether.embedded:NAME` (issue 634 Phase 4 PR 1).
//!
//! ## Identity / runtime split (ADR-0122)
//!
//! The trampoline is split into an addressing **identity** and a
//! state-bearing **runtime**. [`WasmTrampoline`] is a ZST identity carrying
//! only the addressing surface — `Addressable` (`NAMESPACE` / `Resolver`),
//! the per-handler `HandlesKind<DropComponent>` / `HandlesKind<ReplaceComponent>`
//! markers, and the `OnePer("component")` name-inventory entry — all emitted
//! always-on by `#[actor]`. The state-bearing runtime
//! (`WasmTrampolineState`, which owns the wasmtime `Component` plus the
//! `Engine` / `Linker` / `Registry` / `Mailer` / `HubOutbound` handles) and
//! its init config ([`WasmTrampolineConfig`], substrate/wasmtime-typed) live
//! behind the one `feature = "runtime"` gate (the `mod runtime` directory), so
//! a transport-only build of the identity never names the state nor pulls
//! `aether_substrate` through this cap.
//!
//! `WasmTrampoline::NAMESPACE`, `spawn_child::<WasmTrampoline>`, and
//! `resolve_actor::<WasmTrampoline>` resolve against the identity — `spawn_child`
//! / `resolve_actor` bind `A: Instanced + NativeActor`, which is the identity.
//!
//! ## Where this lives (issue 654)
//!
//! The trampoline sits next to [`crate::component::ComponentHostCapability`] —
//! its only consumer — and the namespace is whatever
//! `WasmTrampoline::NAMESPACE` says it is. Single declaration, cap-owned,
//! reachable on every target via the `Addressable` trait const. ADR-0097: the
//! substrate's `TRAMPOLINE_NAMESPACE` forward-feeds the same
//! [`EMBEDDED_SCOPE`] const, collapsing the
//! former two-literal mirror into one source; the
//! `trampoline_namespace_matches_substrate` test guards the match.
//!
//! ## Shape
//!
//! Instanced. Anything the trampoline doesn't handle
//! natively (today: `DropComponent`, `ReplaceComponent`) falls through the
//! `#[fallback]` (`forward_to_wasm`) to the wasm guest via `Component::deliver`.
//! The framework dispatcher reads from the trampoline's `NativeBinding`;
//! un-handled kinds reach `forward_to_wasm`; the guest's `send_mail_p32` /
//! `reply_mail_p32` host fns route through the same binding.
//!
//! ## Lifecycle
//!
//! - **Load**: `crate::component::ComponentHostCapability::on_load_component`
//!   spawns a trampoline via the runtime spawn machinery (subname = the
//!   agent-supplied component name); the spawn path runs `init` which
//!   instantiates the wasm `Component` against the trampoline's binding.
//! - **Drop**: `DropComponent` mail addressed to the trampoline's mailbox
//!   lands on `on_drop_component`, which drops the `Component` and clears the
//!   mailbox's accept-set. The trampoline (and its mailbox name) survives as an
//!   empty slot, refillable by `ReplaceComponent`.
//! - **Replace**: `ReplaceComponent` mail lands on `on_replace_component`,
//!   which instantiates a new `Component` against the same binding and swaps
//!   `state.component`. ADR-0022 + ADR-0038 invariants hold because the inbox
//!   channel is the trampoline's `NativeBinding` and outlives the swap.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated dispatch owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root so the
// `#[actor]`-emitted `impl HandlesKind<K> for WasmTrampoline {}` markers
// (always-on, outside the `feature = "runtime"` gate) resolve. The handler
// *bodies* (gated) reach `DropResult` / `ReplaceResult` through `use runtime::*`.
use aether_actor::{EMBEDDED_SCOPE, actor};
use aether_kinds::{DropComponent, ReplaceComponent};

// The runtime half — the whole `aether_substrate` / `wasmtime`-typed surface
// (imports, `WasmTrampolineState`, `WasmTrampolineConfig`, the replace /
// sibling-spawn helpers) — lives in the `runtime` directory, gated once here.
// The `#[actor] impl` below reaches all of it through the single
// `use runtime::*` glob; the glob is intentional rather than a dozen one-line
// imports.
#[cfg(feature = "runtime")]
mod runtime;

#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

// The init config is substrate/wasmtime-typed (runtime-half), so its
// re-export re-gates to `feature = "runtime"`.
#[cfg(feature = "runtime")]
pub use runtime::WasmTrampolineConfig;

/// The wasm-trampoline **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the `OnePer("component")`
/// name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`WasmTrampolineState`, which holds the wasmtime
/// `Component` and the substrate handles) lives behind the one
/// `feature = "runtime"` gate, so a transport-only build never names the state
/// nor pulls `aether_substrate` through this cap. External consumers address
/// this name — `spawn_child::<WasmTrampoline>`, `resolve_actor::<WasmTrampoline>`,
/// `WasmTrampoline::NAMESPACE`.
pub struct WasmTrampoline;

#[actor(instanced)]
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

    fn init(
        config: WasmTrampolineConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<WasmTrampolineState, BootError> {
        let mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let mut substrate_ctx = ComponentCtx::new(
            mailbox,
            Arc::clone(&config.registry),
            Arc::clone(&mailer),
            Arc::clone(&config.outbound),
        );
        // Wire the trampoline's binding so the guest's reply /
        // outbound-mail host fns route through *this* trampoline's
        // binding (issue 634 Phase 4 PR 3 — single source of inbox
        // truth lives on `NativeBinding`, not on `ComponentCtx`).
        substrate_ctx.install_binding(Arc::clone(ctx.binding()));
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
        .map_err(|e| {
            BootError::Other(io::Error::other(format!("wasm instantiation failed: {e}")).into())
        })?;

        // iamacoffeepot/aether#1128: seed this component's per-handler
        // cost cells from the guest's declared handler set
        // (`config.capabilities`, parsed from the wasm's
        // `aether.kinds.inputs` section). `init` runs inside the spawn
        // path's `with_stamped(&slots, …)`, so the per-actor
        // `CostCells` cache is stamped directly here — the cap's
        // thread vs the trampoline's is irrelevant: the stamp binds to
        // the actor's `ActorSlots`, not to a thread. The same
        // `Arc<CostCell>`s seed the global `CostTable` for the cold
        // `cost.tail` dump and the iamacoffeepot/aether#1178
        // producer-side read. Replace re-seeds on the trampoline's own
        // dispatch (`on_replace_component`); drop clears both indexes.
        let handler_kinds: Vec<KindId> =
            config.capabilities.handlers.iter().map(|h| h.id).collect();
        let seeded = mailer.cost_table().seed(mailbox, &handler_kinds);
        CostCells::try_with_mut(|cells| cells.seed(seeded));

        Ok(WasmTrampolineState {
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
        })
    }

    /// Issue 640 Phase 2: fire the wasm guest's `wire` hook
    /// post-registration. The cap-side spawn flow registers the
    /// trampoline mailbox in step 5–7; this hook runs after that
    /// as part of the dispatcher's lifecycle, so a wire-time
    /// `subscribe_input` mail validates against a live closure
    /// entry. Pre-issue-640 the call lived inside
    /// `Component::instantiate` (step 4, before registration) and
    /// races the input cap's `validate_subscriber_mailbox`,
    /// silently dropping subscribes.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if let Some(component) = state.component.as_mut()
            && let Err(e) = component.wire()
        {
            tracing::error!(
                target: "aether_capabilities::trampoline",
                error = %e,
                "wasm guest `wire` hook returned error",
            );
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
    #[handler]
    fn on_drop_component(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _payload: DropComponent,
    ) -> DropResult {
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
        // iamacoffeepot/aether#1128: drop this mailbox's per-handler
        // cost cells from the global table and the per-actor cache.
        // `on_drop_component` runs on the trampoline's own thread
        // inside `with_stamped`, so both indexes clear together.
        state.mailer.cost_table().drop_mailbox(state.mailbox);
        CostCells::try_with_mut(|cells| cells.seed(Vec::new()));
        DropResult::Ok
    }

    /// Replace the wasm component with a fresh module. ADR-0022 +
    /// ADR-0038 splice invariants hold because the trampoline's
    /// inbox is the framework binding, which outlives the
    /// `Component` swap. `on_dehydrate` runs on the old instance,
    /// `take_saved_state` lifts any rehydration bundle, the new
    /// module instantiates against the same binding, and
    /// `on_rehydrate` runs on the fresh side.
    #[handler]
    fn on_replace_component(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: ReplaceComponent,
    ) -> ReplaceResult {
        state.handle_replace(payload)
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
    fn forward_to_wasm(state: &mut Self::State, ctx: &mut NativeCtx<'_>, env: &Envelope) -> bool {
        // ADR-0097: deliver the inbound, then drain any sibling spawn
        // the guest staged during `deliver`. The block scopes the
        // `&mut component` borrow so `spawn_sibling` can read the
        // trampoline's other fields afterward.
        let pending = {
            let Some(component) = state.component.as_mut() else {
                tracing::warn!(
                    target: "aether_capabilities::trampoline",
                    mailbox = %state.mailbox,
                    kind = %env.kind_name,
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
            let mail = Mail::new(
                env.recipient,
                env.kind,
                env.payload.bytes().to_vec(),
                env.count,
            )
            .with_reply_to(env.sender)
            .with_lineage(env.mail_id, env.root, env.parent_mail);
            if let Err(e) = component.deliver(&mail) {
                // ADR-0063 fail-fast: a wasm trap (or host-fn error
                // returned through `Component::deliver`) kills the
                // substrate. Wedge detection (CPU-loop guests) waits
                // on a future epoch-deadline ADR — symmetric with
                // native actors, which have no wedge guard either
                // today.
                ctx.fatal_abort(format!(
                    "component {} (kind {}) trapped: {e}",
                    state.mailbox, env.kind_name,
                ));
            }
            component.take_pending_spawn()
        };
        if let Some(pending) = pending {
            state.spawn_sibling(ctx, pending);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use aether_actor::{Addressable, EMBEDDED_SCOPE};
    #[cfg(feature = "runtime")]
    use aether_substrate::actor::wasm::component::TRAMPOLINE_NAMESPACE;

    /// ADR-0099 §5/§6, ADR-0119: `WasmTrampoline::NAMESPACE`
    /// (capabilities) forward-feeds [`EMBEDDED_SCOPE`] — `aether-actor`'s sole
    /// owner of the `"aether.embedded"` literal, which sits below this crate.
    /// This guards that the identity ZST keeps resolving to the scope
    /// namespace, so an embedded component registers under and resolves to it.
    #[test]
    fn trampoline_namespace_matches_substrate() {
        assert_eq!(
            <super::WasmTrampoline as Addressable>::NAMESPACE,
            EMBEDDED_SCOPE,
        );
        assert_eq!(EMBEDDED_SCOPE, "aether.embedded");
        // The substrate's `TRAMPOLINE_NAMESPACE` forward-feeds the same const;
        // only reachable when the substrate-typed runtime half is compiled.
        #[cfg(feature = "runtime")]
        assert_eq!(TRAMPOLINE_NAMESPACE, EMBEDDED_SCOPE);
    }
}
