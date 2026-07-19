//! `aether.trace` cap (ADR-0080 §4, slimmed by ADR-0086 Phase 3c).
//!
//! Post-3c this is a thin dispatch cap. It owns the `aether.trace`
//! mailbox solely to service [`DispatchTraced`] — the atomic batched
//! dispatch backing the MCP `send_mail_traced` tool (issue 749). It
//! resolves each envelope's name addressing through the substrate
//! registry and dispatches every spec inheriting the inbound chain, so
//! all children share one root.
//!
//! The trace *fold* it used to host — per-root counters + the parent →
//! mail graph that fed `describe_tree` / `describe_window`, plus the
//! legacy `Settled`-mail emission — retired in ADR-0086:
//!
//! - Settlement moved to the emit-time counter on the chassis
//!   `TraceHandle` (Phase 2): the producer hooks fire `Settled`
//!   synchronously through the `SettlementRegistry`, so this cap is no
//!   longer a settlement authority.
//! - Trace storage decentralized to per-actor rings, queried via
//!   `aether.trace.tail` and stitched client-side by the guided walk
//!   (the sibling [`walk`] module, Phase 3b).
//! - The central `ShardedTraceQueue` + drainer that fed this cap's fold
//!   retired with the fold (Phase 3c) — there is no `BatchedTraceEvents`
//!   stream anymore.

// Handler-signature kind must be importable at module root because
// `#[actor]` emits `impl HandlesKind<DispatchTraced> for X {}` always-on,
// outside the `feature = "runtime"` gate. The reply kind
// (`DispatchTracedAck`) is named only by the gated handler body, so it
// rides the runtime gate below.
use aether_kinds::trace::DispatchTraced;

use aether_actor::actor;

/// Thin `aether.trace` cap **identity** (ADR-0122 identity/runtime
/// split, ADR-0086 Phase 3c). A ZST carrying only the addressing — the
/// `Addressable` / `HandlesKind` markers and the name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`TraceDispatchCapabilityState`, holding the substrate registry
/// handle) lives behind the one `feature = "runtime"` gate, so a
/// transport-only build never names it nor pulls `aether_substrate`
/// through this cap.
///
/// Services [`DispatchTraced`] only; the trace fold + `Settled` emission
/// it used to host retired with the central queue (see module doc).
#[actor(singleton)]
pub struct TraceDispatchCapability;

// The reply kind rides the native gate (not `runtime`): the `#[actor]`
// macro's ADR-0109 `HandlerEntry` inventory submission — emitted on every
// native build, runtime or not — names the handler's reply kind `::ID`,
// so a transport-only build must still see it. The rest of the runtime
// half (the `aether_substrate`-typed imports and the state struct + its
// `with_registry` ctor) sits behind the one `feature = "runtime"` gate.
#[cfg(not(target_family = "wasm"))]
use aether_kinds::trace::DispatchTracedAck;

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `TraceDispatchCapabilityState`, and the `#[runtime] impl`) — lives in
// `runtime.rs`, gated once here. Nothing in this file names a runtime type
// directly, so there is no `use runtime::*` glob (matching `fs/mod.rs`).
#[cfg(feature = "runtime")]
mod runtime;

// ADR-0086 Phase 3b decentralized trace-tree reconstruction: the pure,
// transport-agnostic guided walk + stitch over the per-actor trace rings
// this cap's `aether.trace.tail` serves; the MCP and the in-process
// harness each supply their own fetch. Not an actor, so it lives inside
// the capability it serves — target-agnostic and always-on (folded back
// from the dissolved `aether-rpc` crate per ADR-0124).
pub mod walk;
