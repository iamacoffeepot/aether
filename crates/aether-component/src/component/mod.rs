//! `aether.component` cap (issue 603, renamed in issue 638 phase 3
//! from `aether.control`). The wasm-component lifecycle endpoint:
//! receives [`LoadComponent`](aether_kinds::LoadComponent) mail and spawns a per-component
//! `WasmTrampoline` (issue 634 Phase 4 PR 1) named by the guest's own
//! published namespace: `NS`, `NS:key`, or `parent/NS:key` (ADR-0241 §5).
//! [`DropComponent`](aether_kinds::DropComponent) and
//! [`ReplaceComponent`](aether_kinds::ReplaceComponent) mail flow through the cap as well — it
//! forwards each to the addressed trampoline preserving the
//! original `reply_to`, so the trampoline replies directly to the
//! agent. The trampoline manages its own lifecycle as an instanced
//! [`NativeActor`].
//!
//! Every load and replace first publishes its module (ADR-0241 §3): one
//! registry-owner batch runs admission (§4) and registers the module's kinds,
//! all or nothing. A load spawns, and a replace is forwarded to its
//! trampoline, only once that batch commits; a refusal answers the caller
//! with `module publish refused: …`.
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
//! holding the wasmtime `engine` + `linker`, the registry-inventory
//! subscription, the egress handle, and the default-name counter) lives behind the one
//! `feature = "runtime"` gate. Plain fields (no `Arc<Inner>` wrapper) per
//! ADR-0078 — the cap is single-threaded, every handler runs on the cap's
//! dispatcher thread.
//!
//! The implementation is split across files:
//! - `mod.rs` — this file: the identity ZST, the `#[actor(singleton)] impl
//!   NativeActor` with `init` + the four lifecycle handlers over
//!   `state: &mut Self::State`.
//! - `runtime.rs` — the `feature = "runtime"` half: the state struct and the
//!   substrate / wasmtime imports.
//! - `load.rs` — the `handle_load` sequence as a method on the state; the
//!   state fields carry `pub` so this sibling reaches
//!   them.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// `load` (the `handle_load` sequence) and `config` (the `ComponentHostParams`
// init bundle) now live under the `runtime` directory beside the rest of the
// runtime half, covered by the one `mod runtime;` gate. The cap-root
// re-export sources `ComponentHostParams` through `runtime`.
#[cfg(feature = "runtime")]
pub use runtime::ComponentHostParams;
// The trampoline's replace path checks its hosted type's dependencies
// through the host's shared refusal wording.
#[cfg(feature = "runtime")]
pub(crate) use runtime::replacement_refusal;

// `LoadResult` is named by the emitted reply rows (the load handlers return
// `Pending<LoadResult>`) as well as by the runtime half's own code, so it is
// imported in every build.
use aether_kinds::LoadResult;

// The `#[actor]` attribute sits on the capability struct (the struct-hosted
// ADR-0123 form): it reads the sibling `runtime` module off disk and emits the
// always-on addressing markers + handler inventory against the identity here,
// carrying that module's own imports so the handler and reply kinds resolve
// without being restated at this file's root. Everything that names an
// `aether_substrate` / `wasmtime` type — the `#[runtime] impl NativeActor`, the
// handler/init ctx, and the runtime state — lives in the `runtime` module below,
// gated once by `feature = "runtime"`.
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

/// `aether.component.load_delivered` — the component host hands a successful
/// load to the guest it just staged, which answers the requester with
/// [`LoadResult::Ok`] in its own name.
///
/// The host's held reply rides this mail (`Held::hand_off`): its reply
/// target is the requester and its lineage is the load's chain, so the
/// trampoline's reply settles the requester's call and arrives stamped with
/// the trampoline as sender — the reference the requester keeps (ADR-0230
/// §3). The trampoline answers only the mail's reply target, so a delivery
/// from any other actor reaches only that actor.
#[aether_data::kind(name = "aether.component.load_delivered", no_serde)]
pub struct LoadDelivered {
    /// The loaded component's canonical lineage path.
    pub path: aether_data::ErasedActorPath,
    /// The component's receive-side capabilities (ADR-0033).
    pub capabilities: aether_kinds::ComponentCapabilities,
}

// The runtime half — the whole `aether_substrate` / `wasmtime`-typed surface
// (imports, `ComponentHostCapabilityState`, and the `#[runtime] impl
// NativeActor`) — lives in `runtime.rs`, gated once here. The
// struct-hosted `#[actor]` above reads that module off disk to emit the
// identity markers; the runtime body is self-contained there.
#[cfg(feature = "runtime")]
mod runtime;
