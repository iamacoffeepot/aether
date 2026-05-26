//! Per-pool scheduler mechanism counters (iamacoffeepot/aether#1129).
//!
//! Pure instrumentation: a handful of relaxed [`AtomicU64`] bumped at the
//! four dispatch-mechanism sites the perf methodology can't see through
//! latency percentiles —
//!
//! - **`notify_slow_unparks`** — the producer-side `notify` slow path took
//!   the futex `unpark` (no spinner was available, the ~4.3µs handoff
//!   [`crate::scheduler::SpinPark`] exists to route *away*). The
//!   route-to-spinner fast path bumps nothing.
//! - **`recruit_suppressed`** — a recruit asked for more siblings than the
//!   pool could give and the clamp reduced it. A cost-aware recruiter
//!   (iamacoffeepot/aether#1178) sizing `recruit_k` below the available
//!   group count surfaces here as a suppressed wakeup.
//! - **`steals_injector`** / **`steals_sibling`** — a worker pulled work
//!   from the shared injector vs. raided a sibling's deque tail.
//! - **`inline_runs`** — a wake kept its slot on the producing worker's own
//!   deque (the affinity warm path), so it ran without a futex wakeup.
//!
//! These are near-deterministic for a fixed workload and machine-portable
//! (a wakeup is a wakeup), so — unlike latency, whose run-to-run variance is
//! wakeup-dominated — they support a deterministic, CI-gateable verdict.
//!
//! **Benign-race discipline (mirrors `aether_actor::cost::CostCell` /
//! [`crate::scheduler::calibrate`]).** Every increment is a relaxed
//! `fetch_add`; a lost increment only *undercounts a diagnostic* and never
//! gates a dispatch / recruit / park decision. So the counters add a single
//! relaxed atomic to each hot site and carry no ordering obligation against
//! the lost-wakeup fences they sit beside.

use std::sync::atomic::{AtomicU64, Ordering};

/// Shared, per-pool counter block. Built once at
/// [`crate::scheduler::Pool::start`], wrapped in an `Arc`, and cloned into
/// the [`crate::scheduler::SpinPark`], each worker's deque thread-local, and
/// the [`crate::scheduler::WakeSink`]. Read with [`Self::snapshot`] from any
/// thread.
#[derive(Debug, Default)]
pub struct SchedulerCounters {
    notify_slow_unparks: AtomicU64,
    recruit_suppressed: AtomicU64,
    steals_injector: AtomicU64,
    steals_sibling: AtomicU64,
    inline_runs: AtomicU64,
}

impl SchedulerCounters {
    /// A fresh, all-zero counter block.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One `notify` slow-path futex unpark (no spinner was available).
    pub fn note_notify_slow_unpark(&self) {
        self.notify_slow_unparks.fetch_add(1, Ordering::Relaxed);
    }

    /// A recruit was clamped below what it asked for — `requested - granted`
    /// sibling wakeups were suppressed. `0` (granted >= requested) bumps
    /// nothing.
    pub fn note_recruit_suppressed(&self, suppressed: usize) {
        if suppressed > 0 {
            self.recruit_suppressed
                .fetch_add(suppressed as u64, Ordering::Relaxed);
        }
    }

    /// One steal that pulled from the shared injector.
    pub fn note_steal_injector(&self) {
        self.steals_injector.fetch_add(1, Ordering::Relaxed);
    }

    /// One steal that raided a sibling worker's deque tail.
    pub fn note_steal_sibling(&self) {
        self.steals_sibling.fetch_add(1, Ordering::Relaxed);
    }

    /// One wake that kept its slot on the producing worker's own deque (ran
    /// without a futex wakeup).
    pub fn note_inline_run(&self) {
        self.inline_runs.fetch_add(1, Ordering::Relaxed);
    }

    /// A plain copy of the current counts. Relaxed loads — a snapshot taken
    /// concurrently with an increment may miss the in-flight bump, which is
    /// fine for a profiling delta (the harness brackets a quiesced advance).
    #[must_use]
    pub fn snapshot(&self) -> SchedulerCountersSnapshot {
        SchedulerCountersSnapshot {
            notify_slow_unparks: self.notify_slow_unparks.load(Ordering::Relaxed),
            recruit_suppressed: self.recruit_suppressed.load(Ordering::Relaxed),
            steals_injector: self.steals_injector.load(Ordering::Relaxed),
            steals_sibling: self.steals_sibling.load(Ordering::Relaxed),
            inline_runs: self.inline_runs.load(Ordering::Relaxed),
        }
    }
}

/// A point-in-time copy of [`SchedulerCounters`]. The harness snapshots
/// before/after each cell and records the field-wise delta
/// ([`Self::delta_since`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedulerCountersSnapshot {
    pub notify_slow_unparks: u64,
    pub recruit_suppressed: u64,
    pub steals_injector: u64,
    pub steals_sibling: u64,
    pub inline_runs: u64,
}

impl SchedulerCountersSnapshot {
    /// Field-wise `self - earlier`. Saturating, so a counter that wrapped
    /// (it won't in any realistic run) or an out-of-order read reads `0`
    /// rather than a huge bogus delta.
    #[must_use]
    pub fn delta_since(&self, earlier: &Self) -> Self {
        Self {
            notify_slow_unparks: self
                .notify_slow_unparks
                .saturating_sub(earlier.notify_slow_unparks),
            recruit_suppressed: self
                .recruit_suppressed
                .saturating_sub(earlier.recruit_suppressed),
            steals_injector: self.steals_injector.saturating_sub(earlier.steals_injector),
            steals_sibling: self.steals_sibling.saturating_sub(earlier.steals_sibling),
            inline_runs: self.inline_runs.saturating_sub(earlier.inline_runs),
        }
    }
}
