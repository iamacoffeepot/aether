//! Shared frame-loop policy constants (issue 427).
//!
//! Pre-ADR-0082 this module also held `drain_frame_bound_or_abort`,
//! the per-frame frame-bound drain barrier (ADR-0074 §Decision 5):
//! it polled each frame-bound cap's pending counter under
//! `DRAIN_BUDGET` and routed a counter that wouldn't reach zero
//! through `lifecycle::fatal_abort`. ADR-0082 §6 replaced that poll
//! with settlement gating on the `LifecycleAdvance` chain root — the
//! chassis waits for the frame chain to settle before submit, which
//! covers the same "geometry integrated before submit" invariant via
//! causal completion rather than a pending-counter sweep. The helper
//! (and the `FRAME_BARRIER` machinery that fed it) retired in PR 3c /
//! 3d; `DRAIN_BUDGET` survives as the timeout the settlement wait
//! fatal-aborts against.
//!
//! Issue 775 retired `emit_frame_stats`; issue 634 Phase 4 retired
//! the earlier per-component `drain_or_abort`. `WORKERS` stays
//! chassis-side — post-ADR-0038 it's the declarative wire-stable
//! `EngineInfo.workers` field, not loop policy.

use std::time::Duration;

/// ADR-0063 fail-fast budget for the per-frame settlement wait. A
/// frame chain that doesn't settle within this window is treated as
/// wedged: the chassis loop exits via `lifecycle::fatal_abort`. 5 s
/// is patient enough that ordinary frames don't trip it even on slow
/// first-load compiles, short enough that an operator staring at a
/// frozen window gets a clean exit instead of a multi-minute wait.
pub const DRAIN_BUDGET: Duration = Duration::from_secs(5);
