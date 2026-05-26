//! Boot-time cross-worker handoff cost calibration
//! (iamacoffeepot/aether#1182).
//!
//! The keep-local time valve ([`super::worker_deque::time_budget`],
//! default 12µs) is a proxy for one thing: the cost of *not* inlining a
//! blob — handing it to a parked sibling, which pays a `thread::park` →
//! `unpark` → wake → first cursor-claim round trip before it runs. The
//! 12µs default was derived from a historical ~4.3µs handoff figure
//! (`12 ≈ 3 × 4.3`), but that number was measured once, long ago, on one
//! box. A fixed microsecond budget can't be right on both a fast dev box
//! and the slow CI VM this issue calls out — the handoff cost the valve
//! is supposed to out-amortise is itself box-relative.
//!
//! This module measures that cost directly. At chassis boot a standalone
//! two-thread ping-pong ([`measure_handoff_cost_nanos`]) exercises the
//! real `park` / `unpark` mechanism the scheduler uses, takes the median
//! over many trials (discarding warmup), and caches it process-global
//! ([`handoff_cost`]). The probe models the handoff, not the whole
//! dispatch: a round trip is two `unpark → wake` edges, so one handoff is
//! half a round trip.
//!
//! **This stage is dark.** [`handoff_cost`] drives no scheduling decision
//! yet — [`log_handoff_calibration`] logs the calibrated cost next to the
//! current fixed budget and the *effective multiplier* the fixed budget
//! implies on this box (`current_budget / handoff_cost`), so the box-
//! relative number can be validated across machines before it changes
//! hot-path behavior. If that multiplier clusters across boxes it is the
//! `k` the follow-up wires as `budget = k × handoff_cost`; if it scatters,
//! `k × cost` is the wrong model — which is exactly what this measures. (A
//! single `k` would just bake in the box the default was tuned on: 12µs is
//! `≈ 3 ×` a 4.3µs handoff but `≈ 5.5 ×` a 2.2µs one, so the multiplier is
//! a measurement, not a constant.) The same calibrated cost is the "is
//! parallelism worth the handoff" reference iamacoffeepot/aether#1127's
//! cost-aware recruiter needs, so it is measured once here and exposed for
//! both consumers.

use std::env;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Round trips measured (after warmup). Even, so the median averages the
/// two central samples; large enough to median out scheduler jitter,
/// small enough that the whole probe is sub-millisecond.
const TRIALS: usize = 64;

/// Round trips discarded before sampling, to let both threads' caches and
/// the CPU's frequency scaling settle so the measured edges reflect the
/// steady-state handoff rather than a cold first wake.
const WARMUP: usize = 16;

/// The calibrated cross-worker handoff cost for this process, measured
/// once (lazily) and cached. The keep-local valve and
/// iamacoffeepot/aether#1127's recruiter read this as the box-relative
/// "what does a handoff cost here" reference. `AETHER_HANDOFF_COST_NS`
/// overrides the probe with a fixed nanosecond value (deterministic tests
/// / pinning a known-good number on a noisy box).
#[must_use]
pub fn handoff_cost() -> Duration {
    Duration::from_nanos(handoff_cost_nanos())
}

fn handoff_cost_nanos() -> u64 {
    static COST: OnceLock<u64> = OnceLock::new();
    *COST.get_or_init(resolve_handoff_cost_nanos)
}

fn resolve_handoff_cost_nanos() -> u64 {
    parse_cost_override(env::var("AETHER_HANDOFF_COST_NS").ok())
        .unwrap_or_else(measure_handoff_cost_nanos)
}

/// Parse the `AETHER_HANDOFF_COST_NS` override: a positive integer
/// nanosecond count, else `None` (unset / unparseable / `< 1` all fall
/// back to the live probe). Split out from the env read so the parse is
/// unit-testable without mutating process env.
fn parse_cost_override(raw: Option<String>) -> Option<u64> {
    raw.and_then(|v| v.parse::<u64>().ok()).filter(|&n| n >= 1)
}

/// Log the calibrated handoff cost next to the current fixed keep-local
/// budget and the *effective multiplier* the fixed budget implies on this
/// box (`current_budget / handoff_cost`) — a shadow comparison so the
/// box-relative number can be read out of `actor_logs` on any box
/// (especially the CI VM) before it is wired to drive the valve. The
/// multiplier is the validation signal: clustering across boxes means
/// `budget = k × handoff_cost` is the right model and names `k`; scatter
/// means it isn't. Called once at chassis boot; measures nothing beyond
/// the cached [`handoff_cost`].
pub fn log_handoff_calibration() {
    let cost_ns = u64::try_from(handoff_cost().as_nanos()).unwrap_or(u64::MAX);
    let current_budget_ns =
        u64::try_from(super::worker_deque::time_budget().as_nanos()).unwrap_or(u64::MAX);
    // How many handoffs the current fixed budget spends before spilling on
    // this box. Derived, never asserted — the cross-box clustering of this
    // ratio is what tells the follow-up whether a single `k` exists.
    #[allow(
        clippy::cast_precision_loss,
        reason = "nanosecond counts are well within f64's exact-integer range; the ratio is a diagnostic, not load-bearing"
    )]
    let effective_multiplier = current_budget_ns as f64 / cost_ns as f64;
    tracing::info!(
        target: "aether_substrate::scheduler",
        handoff_cost_ns = cost_ns,
        current_budget_ns,
        effective_multiplier,
        "calibrated cross-worker handoff cost (iamacoffeepot/aether#1182, shadow: not wired to the keep-local valve)",
    );
}

/// Measure the cross-worker handoff cost with a two-thread ping-pong over
/// the real `park` / `unpark` mechanism, returning the median per-handoff
/// nanoseconds (floored to 1).
///
/// Each round: the waker thread stamps `t0`, sets the ping flag and
/// `unpark`s the worker; the worker wakes, sets the pong flag and
/// `unpark`s the waker; the waker wakes and records `t0.elapsed()`. That
/// round trip is **two** `unpark → wake` edges — the same edge a producer
/// pays to hand a blob to a parked sibling — so one handoff is half the
/// round trip. The flags guard each `park` against a spurious return so
/// the lockstep can't desync into a deadlock, and the sticky unpark token
/// means an `unpark` that races ahead of its `park` is not lost.
fn measure_handoff_cost_nanos() -> u64 {
    let rounds = WARMUP + TRIALS;
    // request: waker → worker hand-off signal; reply: worker → waker.
    let request = Arc::new(AtomicBool::new(false));
    let reply = Arc::new(AtomicBool::new(false));
    let waker = thread::current();

    let worker = {
        let request = Arc::clone(&request);
        let reply = Arc::clone(&reply);
        thread::Builder::new()
            .name("aether-handoff-probe".to_string())
            .spawn(move || {
                for _ in 0..rounds {
                    while !request.swap(false, Ordering::Acquire) {
                        thread::park();
                    }
                    reply.store(true, Ordering::Release);
                    waker.unpark();
                }
            })
            .expect("spawn handoff calibration probe thread")
    };

    let mut samples: Vec<u64> = Vec::with_capacity(TRIALS);
    for i in 0..rounds {
        let t0 = Instant::now();
        request.store(true, Ordering::Release);
        worker.thread().unpark();
        while !reply.swap(false, Ordering::Acquire) {
            thread::park();
        }
        // Round trip = two unpark→wake handoffs; one handoff ≈ half.
        let per_handoff = u64::try_from(t0.elapsed().as_nanos() / 2).unwrap_or(u64::MAX);
        if i >= WARMUP {
            samples.push(per_handoff);
        }
    }
    worker
        .join()
        .expect("join handoff calibration probe thread");

    median_nanos(&mut samples).max(1)
}

/// Median of the samples (averaging the two central values for an even
/// count). `0` for an empty slice — the caller floors the result to 1.
fn median_nanos(samples: &mut [u64]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let mid = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        u64::midpoint(samples[mid - 1], samples[mid])
    } else {
        samples[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_handoff_cost_is_plausible() {
        // The probe must return a positive cost, and a sane one — well
        // under 10ms even on a heavily loaded CI VM. The bound is loose on
        // purpose: this guards against a broken probe (zero / absurd),
        // not against box-to-box variance, which is the whole point of
        // measuring per-box.
        let cost = measure_handoff_cost_nanos();
        assert!(cost >= 1, "handoff cost must be a positive floor");
        assert!(
            cost < 10_000_000,
            "handoff cost {cost}ns implausibly large (> 10ms) — probe likely broken",
        );
    }

    #[test]
    fn handoff_cost_is_cached_and_stable() {
        // The accessor caches: repeated reads return the identical value
        // (no re-probe, no drift).
        let a = handoff_cost();
        let b = handoff_cost();
        assert_eq!(a, b, "cached handoff cost must be stable across calls");
        assert!(a >= Duration::from_nanos(1));
    }

    #[test]
    fn cost_override_parses_positive_only() {
        assert_eq!(parse_cost_override(Some("5000".to_string())), Some(5000));
        assert_eq!(
            parse_cost_override(Some("0".to_string())),
            None,
            "zero falls back to the live probe",
        );
        assert_eq!(parse_cost_override(Some("nope".to_string())), None);
        assert_eq!(parse_cost_override(None), None);
    }

    #[test]
    fn median_handles_even_odd_and_empty() {
        assert_eq!(median_nanos(&mut []), 0);
        assert_eq!(median_nanos(&mut [7]), 7);
        // Odd: central element after sort.
        assert_eq!(median_nanos(&mut [9, 1, 5]), 5);
        // Even: average of the two central elements after sort.
        assert_eq!(median_nanos(&mut [10, 2, 8, 4]), 6);
    }
}
