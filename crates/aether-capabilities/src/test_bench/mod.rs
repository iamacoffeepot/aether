//! `aether.test_bench` cap stub (issue 603 Phase 4).
//!
//! The test-bench chassis hosts a real `TestBenchCapability` (in
//! `aether-substrate-bundle::test_bench`) that dispatches `Advance`
//! by pushing to the embedder's event channel. Desktop and headless
//! don't drive ticks via `aether.test_bench.advance` — they have
//! their own frame loops — so they compose this cap to fail-fast
//! with `Err`-replies instead of letting the mail warn-drop and
//! hang the agent's await-reply slot.
//!
//! Mirrors the pattern from `HeadlessRenderCapability` /
//! `HeadlessWindowCapability`: same mailbox name across chassis,
//! cap variants per chassis profile.

// Handler-signature kinds resolve at file root — `#[actor]` emits the
// `impl HandlesKind<K> for X {}` markers always-on against the identity,
// outside the `feature = "runtime"` gate, so they reference these kinds
// from here. `AdvanceResult` is used in the `on_advance` handler body
// inside the `#[runtime] impl` in the sibling `runtime.rs`.
use aether_kinds::{Advance, AdvanceResult};

use aether_actor::actor;

/// Stub cap for `aether.test_bench` on chassis without test-bench drive
/// (desktop, headless). Replies `AdvanceResult::Err` so MCP
/// `aether.test_bench.advance` mail fails fast instead of hanging on a
/// reply that never comes (ADR-0122 identity/runtime split).
#[actor(singleton)]
pub struct UnsupportedTestBenchCapability;

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `UnsupportedTestBenchCapabilityState`, and the `#[runtime] impl`) — lives
// in `runtime.rs`, gated once here. Nothing in this file names a runtime
// type directly, so there is no `use runtime::*` glob (matching
// `fs/mod.rs`).
#[cfg(feature = "runtime")]
mod runtime;
