//! `aether.component` cap (issue 603, renamed in issue 638 phase 3
//! from `aether.control`). The wasm-component lifecycle endpoint:
//! receives [`LoadComponent`] mail and spawns a per-component
//! `WasmTrampoline` (issue 634 Phase 4 PR 1) addressed at
//! `aether.embedded:NAME`. [`DropComponent`] and
//! [`ReplaceComponent`] mail flow through the cap as well — it
//! forwards each to the addressed trampoline preserving the
//! original `reply_to`, so the trampoline replies directly to the
//! agent. The cap holds no per-component bookkeeping; the
//! trampoline manages its own lifecycle as an instanced [`NativeActor`].
//!
//! Pre-Phase-4 the cap also owned the wasm dispatcher infrastructure
//! (the retired `ComponentEntry`, `dispatcher_loop`, `kill_actor`,
//! `splice_inbox`, etc.) and installed itself as the `Mailer`'s
//! `ComponentRouter` for component-bound routing. All of that
//! retired with the trampoline migration: dispatch lives on the
//! framework's `NativeActor` loop, replace is `Component`-swap
//! inside the trampoline, drop flows through `ctx.shutdown()`.
//!
//! [`NativeActor`]: aether_substrate::NativeActor
//!
//! The cap follows the ADR-0122 identity/runtime split (the `aether.fs`
//! worked example, #2318): the addressing identity is the ZST
//! [`ComponentHostCapability`] — the `#[actor(singleton)]` markers
//! (`Addressable`, the per-handler `HandlesKind`, the name inventory) ride it
//! always-on, so a transport-only build addresses the cap without naming the
//! substrate-typed state. The state-bearing runtime
//! (`ComponentHostCapabilityState`,
//! holding the wasmtime `engine` + `linker`, the `registry`, the egress
//! handles, and the default-name counter) lives behind the one
//! `feature = "runtime"` gate. Plain fields (no `Arc<Inner>` wrapper) per
//! ADR-0078 — the cap is single-threaded, every handler runs on the cap's
//! dispatcher thread.
//!
//! The implementation is split across files:
//! - `mod.rs` — this file: the identity ZST, the `#[actor(singleton)] impl
//!   NativeActor` with `init` + the four lifecycle handlers over
//!   `state: &mut Self::State`.
//! - `runtime.rs` — the `feature = "runtime"` half: the state struct, the
//!   substrate / wasmtime imports, and the free `forward_to_trampoline`.
//! - `route.rs` — the send-side peer-addressing facades
//!   ([`ComponentHostWasmExt`], [`ComponentHostNativeExt`],
//!   [`resolve_embedded`]).
//! - `load.rs` — the `handle_load` sequence as a method on the state; the
//!   state fields carry `pub(in crate::component)` so this sibling reaches
//!   them.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

mod route;
#[cfg(not(target_arch = "wasm32"))]
pub use route::ComponentHostNativeExt;
pub use route::{ComponentHostWasmExt, resolve_embedded};

#[cfg(feature = "runtime")]
mod load;

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::ComponentHostConfig;

// Handler-signature kinds resolve at file root always-on: `#[actor]` emits the
// `impl HandlesKind<K>` markers AND the `aether.kinds.inputs` handler-inventory
// (which names each handler's reply kind via `<R as Kind>::ID`) against the
// identity, outside the `feature = "runtime"` gate — so both the input kinds
// and the reply kinds must be in scope here, not behind the runtime gate.
use aether_kinds::{
    DropComponent, ListComponents, ListComponentsResult, LoadComponent, LoadResult,
    ReplaceComponent,
};

// The crate-local wiring the `#[actor] impl` handler bodies name (sibling caps,
// the unsubscribe kind, the `Kind` / `MailboxCategory` vocabulary) lives in
// `mod runtime` and reaches here through the `use runtime::*` glob below.

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` /
// `wasmtime` type — the handler/init ctx, the runtime state, the
// `forward_to_trampoline` helper — lives in the `runtime` module below, gated
// once by `feature = "runtime"` and written cfg-free within; the `#[actor]
// impl` reaches all of it through the single `use runtime::*` glob.
use aether_actor::actor;

// The `runtime` module is this cap's private runtime-half namespace; the impl
// reaches all of it (state, ctx types, the forward helper) through this single
// seam, so the glob is intentional rather than a dozen one-line imports.
#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

/// `aether.component` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry, all
/// emitted always-on by `#[actor]`. The state-bearing runtime
/// (`ComponentHostCapabilityState`, holding the wasmtime `engine` + `linker`
/// and the egress handles) lives behind the one `feature = "runtime"` gate, so
/// a transport-only build never names the state nor pulls `aether_substrate` /
/// `wasmtime` through this cap.
pub struct ComponentHostCapability;

#[actor(singleton)]
impl NativeActor for ComponentHostCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// wasmtime instances, mail registry, egress handles, and default-name
    /// counter every load instantiates against.
    type State = ComponentHostCapabilityState;

    type Config = ComponentHostConfig;
    const NAMESPACE: &'static str = "aether.component";

    fn init(
        config: ComponentHostConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ComponentHostCapabilityState, BootError> {
        let mailer = ctx.mailer();
        let registry = Arc::clone(mailer.registry());
        Ok(ComponentHostCapabilityState {
            engine: config.engine,
            linker: config.linker,
            registry,
            mailer,
            outbound: config.hub_outbound,
            default_name_counter: AtomicU64::new(0),
        })
    }

    /// Load a fresh wasm component into the substrate.
    ///
    /// # Agent
    /// Pass the wasm bytes plus an optional `name`. On Ok the cap
    /// registers the kinds the wasm declared in its `aether.kinds`
    /// section, picks a final name (caller value > wasm's
    /// `aether.namespace` > `component_N`), spawns a
    /// [`WasmTrampoline`](crate::trampoline::WasmTrampoline) under
    /// `aether.embedded:NAME`, and replies `LoadResult::Ok { mailbox_id,
    /// name, capabilities }` where `name` is the full trampoline
    /// address — agents send subsequent mail to that name.
    /// Errors (bad wire bytes, kind conflict, name conflict,
    /// invalid wasm, instantiation trap) come back as
    /// `LoadResult::Err`.
    #[handler]
    fn on_load_component(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: LoadComponent,
    ) -> LoadResult {
        // ADR-0109: the return type is the reply contract — the
        // `#[actor]` macro routes this `LoadResult` back to the sender
        // through `OutboundReply::reply`, so no manual `ctx.reply`.
        state.handle_load(ctx, payload)
    }

    /// Drop a component by its mailbox id. Forwards
    /// [`DropComponent`] mail to the addressed trampoline; the
    /// trampoline's `WasmTrampoline::on_drop_component` handler
    /// replies `DropResult::Ok` and shuts itself down.
    ///
    /// Before forwarding, purges the dying trampoline's mailbox from
    /// every fan-out subscriber table so no cap keeps firing at a
    /// dropped mailbox: `aether.input`'s input-stream tables (via
    /// [`UnsubscribeAll`]) and `aether.lifecycle`'s per-stage tables
    /// (via [`LifecycleUnsubscribeAll`]).
    ///
    /// # Agent
    /// `DropComponent { mailbox_id }`. The `mailbox_id` is the
    /// trampoline's id from the `LoadResult.mailbox_id` field.
    #[handler]
    fn on_drop_component(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: DropComponent,
    ) {
        // Cap-side cleanup: ask each owning cap to drop the dying
        // trampoline from its fan-out sets. Mail rather than direct
        // mutation post-issue-640 — each cap is the sole owner of its
        // own subscriber table.
        ctx.actor::<InputCapability>().send(&UnsubscribeAll {
            mailbox: payload.mailbox_id,
        });
        ctx.actor::<LifecycleCapability>()
            .send(&LifecycleUnsubscribeAll {
                mailbox: payload.mailbox_id.0,
            });
        forward_to_trampoline(ctx, payload.mailbox_id, DropComponent::ID, &payload);
    }

    /// Replace the component at `mailbox_id` with a fresh wasm
    /// binary. Forwards [`ReplaceComponent`] to the trampoline;
    /// the trampoline's `WasmTrampoline::on_replace_component`
    /// handler swaps `Component` internally and replies
    /// `ReplaceResult`. ADR-0022 + ADR-0038 splice invariants
    /// hold because the inbox channel is the trampoline's
    /// `NativeBinding`, which outlives the swap.
    ///
    /// # Agent
    /// `ReplaceComponent { mailbox_id, wasm, drain_timeout_ms, config, export }`.
    /// `drain_timeout_ms` is accepted for wire compatibility but
    /// ignored under the trampoline's binding-stable replace.
    /// `export` (ADR-0096) names which exported actor type of the
    /// replacement module to instantiate; `None` reuses the type the
    /// trampoline currently hosts.
    #[handler]
    fn on_replace_component(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: ReplaceComponent,
    ) {
        forward_to_trampoline(ctx, payload.mailbox_id, ReplaceComponent::ID, &payload);
    }

    /// Enumerate the components this engine has actually loaded and
    /// registered, by their ADR-0099 lineage names (issue 2020).
    ///
    /// Reads the registry's live mailbox snapshot — the same list
    /// already egressed to the hub after each load — and keeps only the
    /// [`MailboxCategory::Trampoline`] entries, the loaded-component set.
    /// Chassis caps are boot-present and static, so the trampolines are
    /// the only registry membership a readiness poll cares about. The
    /// reply is names only: the mailbox id is a deterministic hash-chain
    /// over the lineage the name renders (ADR-0099) and routing is the
    /// substrate's job, so the caller never needs the handle.
    ///
    /// # Agent
    /// Fieldless `ListComponents` to the `aether.component` mailbox —
    /// guaranteed present from boot, so the send always resolves and the
    /// reply is a definitive snapshot. Reply `ListComponentsResult {
    /// names }` lists every currently-loaded component's full lineage
    /// address (`aether.component/aether.embedded:NAME`). Poll it after a
    /// boot-manifest spawn (ADR-0116) to learn deterministically when a
    /// requested component is loaded, instead of inferring liveness by
    /// proxy.
    #[handler]
    fn on_list_components(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _payload: ListComponents,
    ) -> ListComponentsResult {
        let names = state
            .registry
            .list_mailbox_descriptors()
            .into_iter()
            .filter(|d| d.category == Some(MailboxCategory::Trampoline))
            .map(|d| d.name)
            .collect();
        ListComponentsResult { names }
    }
}

// The runtime half — the whole `aether_substrate` / `wasmtime`-typed surface
// (imports, `ComponentHostCapabilityState`, `forward_to_trampoline`) — lives
// in `runtime.rs`, gated once here. The `#[actor] impl` above reaches it
// through the `use runtime::*` glob.
#[cfg(feature = "runtime")]
mod runtime;

#[cfg(test)]
mod tests {
    // These tests construct the host carry and assert the canonical
    // trampoline-address fold against the flat name hash — the primitive is
    // the reference value under test, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use aether_actor::wasm::inline::InlineRegistry;
    use aether_actor::{Addressable, WasmActorMailbox};
    use aether_data::{ActorId, MailboxId, Tag, fold_lineage, mailbox_id_from_name, with_tag};

    use super::{ComponentHostCapability, ComponentHostWasmExt};
    use crate::trampoline::WasmTrampoline;

    /// A loaded component's id is the ADR-0099 §3 lineage fold over
    /// `[aether.component, aether.embedded:<name>]`. `loaded`
    /// composes exactly that — folding the trampoline node's `ActorId`
    /// onto the component host's carry — so it agrees with the id the
    /// spawn machinery registers it under. It must **not** resolve the
    /// bare load-name (`ctx.actor::<R>()` hashing the bare `NAMESPACE`),
    /// nor the pre-0099 flat `trampoline:<name>` hash — both reach a
    /// mailbox nothing is registered under (the #1364 footgun). This pins
    /// the one canonical path for a loaded component.
    #[test]
    fn loaded_composes_the_canonical_trampoline_address() {
        // `R` is arbitrary here — the resolved id depends only on the
        // host carry + the trampoline node name. The ctx binding (sender +
        // inline registry) is irrelevant to id resolution, so a throwaway
        // registry and a zero sender suffice (issue 1987).
        let registry = InlineRegistry::new();
        let host = WasmActorMailbox::<ComponentHostCapability>::__new(
            mailbox_id_from_name(ComponentHostCapability::NAMESPACE).0,
            0,
            &registry,
        );
        let camera = host.loaded::<ComponentHostCapability>("aether.camera");

        // The component host is root-pinned (depth-1), so its carry is
        // its own id; fold the trampoline node onto it.
        let host_carry = mailbox_id_from_name(ComponentHostCapability::NAMESPACE).0;
        let node = ActorId::instanced(WasmTrampoline::NAMESPACE, "aether.camera");
        let canonical = MailboxId(with_tag(Tag::Mailbox, fold_lineage(host_carry, node)));
        assert_eq!(camera.mailbox_id(), canonical);

        // Not the pre-0099 flat name-hash, and not the bare load-name.
        assert_ne!(
            camera.mailbox_id(),
            mailbox_id_from_name(&format!("{}:camera", WasmTrampoline::NAMESPACE)),
        );
        assert_ne!(camera.mailbox_id(), mailbox_id_from_name("camera"));
    }

    /// Root singletons (chassis caps) are unchanged by the scoped-name
    /// composition: the cap's own mailbox id is its bare `NAMESPACE`.
    #[test]
    fn root_singleton_id_is_the_bare_namespace() {
        let registry = InlineRegistry::new();
        let host = WasmActorMailbox::<ComponentHostCapability>::__new(
            mailbox_id_from_name(ComponentHostCapability::NAMESPACE).0,
            0,
            &registry,
        );
        assert_eq!(
            host.mailbox_id(),
            mailbox_id_from_name(ComponentHostCapability::NAMESPACE),
        );
    }
}
