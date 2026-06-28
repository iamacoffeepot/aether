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
#[cfg(not(target_family = "wasm"))]
pub use route::ComponentHostNativeExt;
pub use route::{ComponentHostWasmExt, resolve_embedded};

// `load` (the `handle_load` sequence) and `config` (the `ComponentHostConfig`
// init bundle) now live under the `runtime` directory beside the rest of the
// runtime half, covered by the one `mod runtime;` gate. The cap-root
// re-export sources `ComponentHostConfig` through `runtime`.
#[cfg(feature = "runtime")]
pub use runtime::ComponentHostConfig;

// Handler-signature kinds resolve at file root always-on: `#[actor]` emits the
// `impl HandlesKind<K>` markers AND the `aether.kinds.inputs` handler-inventory
// (which names each handler's reply kind via `<R as Kind>::ID`) against the
// identity, outside the `feature = "runtime"` gate — so both the input kinds
// and the reply kinds must be in scope here, not behind the runtime gate.
use aether_kinds::{
    DescribeComponent, DescribeComponentResult, DropComponent, ListComponents,
    ListComponentsResult, LoadComponent, LoadResult, ReplaceComponent,
};

// The `#[actor]` attribute sits on the capability struct (the struct-hosted
// ADR-0123 form): it reads the sibling `runtime` module off disk and emits the
// always-on addressing markers + handler inventory against the identity here.
// Everything that names an `aether_substrate` / `wasmtime` type — the
// `#[runtime] impl NativeActor`, the handler/init ctx, the runtime state, the
// `forward_to_trampoline` helper — lives in the `runtime` module below, gated
// once by `feature = "runtime"`; the body sources those names beside itself, so
// only the handler-argument kinds the emitted markers lift verbatim must keep
// resolving at this file's root (the `aether_kinds` import above).
use aether_actor::actor;

/// `aether.component` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry, all
/// emitted always-on by `#[actor]`. The state-bearing runtime
/// (`ComponentHostCapabilityState`, holding the wasmtime `engine` + `linker`
/// and the egress handles) lives behind the one `feature = "runtime"` gate, so
/// a transport-only build never names the state nor pulls `aether_substrate` /
/// `wasmtime` through this cap.
#[actor(singleton)]
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
}
