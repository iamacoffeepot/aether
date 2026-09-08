//! `aether.component` cap (issue 603, renamed in issue 638 phase 3
//! from `aether.control`). The wasm-component lifecycle endpoint:
//! receives [`LoadComponent`](aether_kinds::LoadComponent) mail and spawns a per-component
//! `WasmTrampoline` (issue 634 Phase 4 PR 1) addressed at
//! `aether.embedded:NAME`. [`DropComponent`](aether_kinds::DropComponent) and
//! [`ReplaceComponent`](aether_kinds::ReplaceComponent) mail flow through the cap as well — it
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
//! [`ComponentHostCapability`] — the `#[actor(singleton, root)]` markers
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
//! - `route.rs` — the by-name address supplier [`resolve_embedded`], for a
//!   caller with no co-hosted ctx to resolve from. A co-hosted actor addresses
//!   a loaded component by type instead (`ctx.actor::<R>()`, or
//!   `ctx.resolve_embedded::<R>(load_name)` for an explicit load name), which
//!   accepts only `Addressable<Resolver = Embedded>` recipients — the
//!   placement the physical trampoline mailbox has.
//! - `load.rs` — the `handle_load` sequence as a method on the state; the
//!   state fields carry `pub` so this sibling reaches
//!   them.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

mod route;
pub use route::resolve_embedded;

// `load` (the `handle_load` sequence) and `config` (the `ComponentHostParams`
// init bundle) now live under the `runtime` directory beside the rest of the
// runtime half, covered by the one `mod runtime;` gate. The cap-root
// re-export sources `ComponentHostParams` through `runtime`.
#[cfg(feature = "runtime")]
pub use runtime::ComponentHostParams;

// `LoadResult` is named by the runtime half's own code, not by the emitted
// markers, so it keeps the runtime gate the rest of that half rides.
#[cfg(feature = "runtime")]
use aether_kinds::LoadResult;

// The `#[actor]` attribute sits on the capability struct (the struct-hosted
// ADR-0123 form): it reads the sibling `runtime` module off disk and emits the
// always-on addressing markers + handler inventory against the identity here,
// carrying that module's own imports so the handler and reply kinds resolve
// without being restated at this file's root. Everything that names an
// `aether_substrate` / `wasmtime` type — the `#[runtime] impl NativeActor`, the
// handler/init ctx, the runtime state, the `forward_to_trampoline` helper —
// lives in the `runtime` module below, gated once by `feature = "runtime"`.
use aether_actor::actor;

/// `aether.component` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry, all
/// emitted always-on by `#[actor]`. The state-bearing runtime
/// (`ComponentHostCapabilityState`, holding the wasmtime `engine` + `linker`
/// and the egress handles) lives behind the one `feature = "runtime"` gate, so
/// a transport-only build never names the state nor pulls `aether_substrate` /
/// `wasmtime` through this cap.
#[actor(singleton, root)]
pub struct ComponentHostCapability;

// The runtime half — the whole `aether_substrate` / `wasmtime`-typed surface
// (imports, `ComponentHostCapabilityState`, `forward_to_trampoline`, and the
// `#[runtime] impl NativeActor`) — lives in `runtime.rs`, gated once here. The
// struct-hosted `#[actor]` above reads that module off disk to emit the
// identity markers; the runtime body is self-contained there.
#[cfg(feature = "runtime")]
mod runtime;

#[cfg(test)]
mod tests {
    // These tests construct the host carry and assert the canonical
    // trampoline-address fold against the flat name hash — the primitive is
    // the reference value under test, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use aether_actor::wasm::NO_INBOUND_SOURCE;
    use aether_actor::wasm::inline::Registry as InlineRegistry;
    use aether_actor::{Addressable, Embedded, Manual, Resolve, WasmActorMailbox, WasmCtx};
    use aether_data::mailbox_id_from_name;
    use aether_substrate::mail::registry::{Registry, noop_handler};
    use aether_substrate::testing::boot_authority;

    use super::{ComponentHostCapability, resolve_embedded};
    use crate::trampoline::WasmTrampoline;

    struct Guest;

    impl Addressable for Guest {
        const NAMESPACE: &'static str = "aether.kit.camera";
        type Resolver = Embedded;
    }

    /// Tripwire: a loaded component's id is the ADR-0099 §3 lineage fold over
    /// `[aether.component, aether.embedded:<name>]`, and the cap registers its
    /// trampoline at that id. Bare-type addressing from a co-hosted ctx, the
    /// named form, the declared host-to-trampoline edge, and this cap's
    /// by-name `resolve_embedded` must therefore all land on it — a change to
    /// the fold that misses any one of them splits the address the host
    /// registers from the address senders compute.
    #[test]
    fn typed_and_by_name_routes_compose_the_canonical_trampoline_address() {
        // The ctx binding (sender + inline registry) is irrelevant to id
        // resolution, so a throwaway registry and a zero sender suffice
        // (issue 1987).
        let registry = InlineRegistry::new();
        let caller = resolve_embedded("test.component.caller");
        let parent = mailbox_id_from_name(ComponentHostCapability::NAMESPACE);
        registry.set_self_id(caller.0);
        registry.set_parent_id(parent.0);
        let host = WasmActorMailbox::<ComponentHostCapability>::__new(parent.0, 0, &registry);
        let name = Guest::NAMESPACE;
        let trampoline = host.resolve::<WasmTrampoline>(name);
        let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(caller.0, &registry, NO_INBOUND_SOURCE);

        assert_eq!(ctx.actor::<Guest>().mailbox_id(), trampoline.mailbox_id());
        assert_eq!(ctx.actor::<Guest>().mailbox_id(), resolve_embedded(name));
        assert_eq!(ctx.resolve_embedded::<Guest>(name).mailbox_id(), trampoline.mailbox_id());
    }

    /// Tripwire: typed lookup follows the parent mailbox injected into each
    /// runtime instance, not the caller's own. The same guest type therefore
    /// resolves beneath nested host instances and changes address when
    /// re-parented, while the default and named spellings share one resolver
    /// path.
    #[test]
    fn typed_lookup_follows_nested_and_reparented_runtime_parents() {
        let parent_a = aether_data::mailbox_id_from_path("test.root/test.composite:a");
        let parent_b = aether_data::mailbox_id_from_path("test.root/test.composite:b");
        let caller_a = Embedded::resolve(parent_a.0, "caller", ());
        let caller_b = Embedded::resolve(parent_b.0, "caller", ());
        let registry_a = InlineRegistry::new();
        registry_a.set_self_id(caller_a.0);
        registry_a.set_parent_id(parent_a.0);
        let registry_b = InlineRegistry::new();
        registry_b.set_self_id(caller_b.0);
        registry_b.set_parent_id(parent_b.0);
        let ctx_a: WasmCtx<'_, Manual> = WasmCtx::__new(caller_a.0, &registry_a, NO_INBOUND_SOURCE);
        let ctx_b: WasmCtx<'_, Manual> = WasmCtx::__new(caller_b.0, &registry_b, NO_INBOUND_SOURCE);

        assert_eq!(ctx_a.actor::<Guest>().mailbox_id(), Embedded::resolve(parent_a.0, Guest::NAMESPACE, ()));
        assert_eq!(ctx_b.actor::<Guest>().mailbox_id(), Embedded::resolve(parent_b.0, Guest::NAMESPACE, ()));
        assert_eq!(
            ctx_a.resolve_embedded::<Guest>("camera-7").mailbox_id(),
            Embedded::resolve(parent_a.0, "camera-7", ())
        );
        assert_eq!(
            ctx_b.resolve_embedded::<Guest>("camera-7").mailbox_id(),
            Embedded::resolve(parent_b.0, "camera-7", ())
        );
        assert_ne!(ctx_a.actor::<Guest>().mailbox_id(), ctx_b.actor::<Guest>().mailbox_id());
    }

    /// The external registry boundary expands abbreviated component
    /// addresses before its canonical live lookup. Typed resolution,
    /// the full canonical path, the short discriminator, and the
    /// explicit child segment therefore identify one mailbox, while
    /// reverse lookup retains only the canonical spelling.
    #[test]
    fn registry_resolves_typed_canonical_and_abbreviated_component_addresses_equally() {
        let inline_registry = InlineRegistry::new();
        let host = WasmActorMailbox::<ComponentHostCapability>::__new(
            mailbox_id_from_name(ComponentHostCapability::NAMESPACE).0,
            0,
            &inline_registry,
        );
        let name = "camera";
        let typed = host.resolve::<WasmTrampoline>(name).mailbox_id();
        let canonical = format!("{}/{}:{name}", ComponentHostCapability::NAMESPACE, WasmTrampoline::NAMESPACE);
        let registry = Registry::new();
        registry
            .try_register_inbox_with_id(&boot_authority(), typed, canonical.clone(), noop_handler())
            .expect("register canonical trampoline mailbox");

        for address in [canonical.as_str(), "aether.component://camera", "aether.component://aether.embedded:camera"] {
            let resolved = registry.resolve_address(address).expect("address resolves to the live trampoline");
            assert_eq!(resolved.mailbox_id, typed);
            assert_eq!(resolved.canonical_path, canonical);
        }
        assert_eq!(registry.mailbox_name(typed).as_deref(), Some(canonical.as_str()));
        assert!(
            registry.list_mailbox_descriptors().iter().all(|descriptor| !descriptor.name.contains("://")),
            "alias spellings never enter registry inventory"
        );
    }
}
