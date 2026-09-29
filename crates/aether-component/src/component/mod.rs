//! `aether.component` cap (issue 603, renamed in issue 638 phase 3
//! from `aether.control`). The wasm-component lifecycle endpoint and the
//! front door that publishes and spawns code by mail (ADR-0241 §9):
//! [`Publish`](aether_kinds::Publish) binds a module's namespaces,
//! [`Spawn`](aether_kinds::Spawn) asks for an instance of a published type,
//! which runs in a per-component `WasmTrampoline` (issue 634 Phase 4 PR 1)
//! named by the guest's own published namespace: `NS`, `NS:key`, or
//! `parent/NS:key` (ADR-0241 §5). A live name answers with the instance
//! there, and [`LoadComponent`](aether_kinds::LoadComponent) is a publish
//! then a spawn.
//! [`DropComponent`](aether_kinds::DropComponent) mail flows through the cap
//! as well — it hands each to the addressed trampoline with the original
//! caller as its reply target, so the trampoline replies directly to the
//! agent. The trampoline manages its own lifecycle as an instanced
//! [`NativeActor`]: a drop closes it and its name tombstones (ADR-0241 §8),
//! and the host refuses a later drop at that path.
//!
//! Every publish, and so every load, runs one path (ADR-0241 §3): a module
//! already published changes nothing, a first publish runs admission (§4)
//! and registers the module's kinds in one registry-owner batch, all or
//! nothing, and a successor republishes as one group (§7): the host
//! pre-checks it against every live instance of the module's namespaces,
//! drives each through the trampoline's prepare, then publishes the module
//! and commits every instance, or aborts every one. A load spawns only once
//! its module is bound; a refusal answers the caller with the reason.
//! [`ReplaceComponent`](aether_kinds::ReplaceComponent) is a publish of a
//! successor answered as a replace.
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
//! - `mod.rs` — this file: the identity ZST and the always-on control rows.
//! - `runtime/` — the `feature = "runtime"` half: the state struct, the
//!   `#[runtime] impl NativeActor` and its handlers, and one file or
//!   directory per door: `publish`, `spawn`, `load` (the spawn half every
//!   door shares), and `republish`.

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

/// `aether.component.spawn_delivered` — the component host hands a spawn to
/// the guest it names, which answers the requester in its own name:
/// [`SpawnResult::Live`](aether_kinds::SpawnResult::Live) when it was
/// already live, [`SpawnResult::Spawned`](aether_kinds::SpawnResult::Spawned)
/// when the spawn just stood it up.
///
/// The host's held reply rides this mail as it rides [`LoadDelivered`], so
/// the requester keeps the reply's stamped sender as its reference (ADR-0230
/// §3).
#[aether_data::kind(name = "aether.component.spawn_delivered", no_serde)]
pub struct SpawnDelivered {
    /// The instance's canonical lineage path.
    pub path: aether_data::ErasedActorPath,
    /// The instance's receive-side capabilities (ADR-0033).
    pub capabilities: aether_kinds::ComponentCapabilities,
    /// Whether the instance was live before the spawn arrived.
    pub live: bool,
}

// The prepare, commit and abort rows a republish drives each guest through
// (ADR-0241 §7), always compiled in beside `LoadDelivered`.
mod control;

pub use control::{Abort, Aborted, Commit, Committed, Prepare, Prepared};

// The runtime half — the whole `aether_substrate` / `wasmtime`-typed surface
// (imports, `ComponentHostCapabilityState`, and the `#[runtime] impl
// NativeActor`) — lives in `runtime.rs`, gated once here. The
// struct-hosted `#[actor]` above reads that module off disk to emit the
// identity markers; the runtime body is self-contained there.
#[cfg(feature = "runtime")]
mod runtime;
