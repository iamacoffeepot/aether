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
//! This module measures that cost directly, in two stages that feed one
//! process-global estimate ([`handoff_cost`]):
//!
//! 1. **Boot probe** (iamacoffeepot/aether#1182 Part 1). At chassis boot a
//!    standalone two-thread ping-pong ([`measure_handoff_cost_nanos`])
//!    exercises the real `park` / `unpark` mechanism, takes the median over
//!    many trials (discarding warmup), and *seeds* the estimate. The probe
//!    models the handoff, not the whole dispatch: a round trip is two
//!    `unpark → wake` edges, so one handoff is half a round trip.
//! 2. **Live refinement** (Part 2). Every genuine parked-worker wake folds
//!    its measured `notify → wake` latency into the same estimate via a
//!    constant-α EWMA ([`fold_handoff_sample`], called from
//!    [`super::spin_park`]). The boot probe measures the handoff *idle* (a
//!    clean 2-thread ping-pong); live samples capture it under the real
//!    operating load (cache pressure, contention, the full worker set), so
//!    the estimate converges on the handoff the valve actually has to
//!    out-amortise rather than the best case. `AETHER_HANDOFF_COST_NS`
//!    pins the estimate to a fixed value and freezes live refinement
//!    (deterministic tests / a known-good number on a noisy box).
//!
//! [`handoff_cost`] now **drives the keep-local time valve**
//! (iamacoffeepot/aether#1182): [`super::worker_deque::time_budget`] is a
//! small multiple of it (`BUDGET_HANDOFF_MULTIPLIER`, clamped) instead of
//! the prior fixed 12µs, so the valve out-amortises the *measured*
//! per-box handoff rather than a one-box constant. That multiple was
//! chosen from the box-relative `current_budget / handoff_cost` shadow the
//! earlier dark stages logged: 12µs was `≈ 6 ×` this box's ~2µs handoff
//! (not the `≈ 3 ×` a historical 4.3µs implied), so a single hardcoded `k`
//! would have baked in whichever box it was tuned on — wiring it to the
//! measurement is what makes it portable. [`log_handoff_calibration`]
//! still logs the realized budget at boot for cross-box validation. The
//! same calibrated cost is the "is parallelism worth the handoff"
//! reference iamacoffeepot/aether#1127's cost-aware recruiter needs, so it
//! is measured once here and read by both consumers.

use std::env;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::cost::ewma_step;

use crate::config::{KnobKind, KnobRecord};

/// Discovery records for the handoff-cost calibration knob (ADR-0090
/// unit b2). Describes the `AETHER_HANDOFF_COST_NS` override read in
/// [`ensure_seeded`] so the e1 sweep / e2 dump cover it; the seed path
/// stays untouched.
pub const CALIBRATE_KNOBS: &[KnobRecord] = &[KnobRecord {
    env_key: "AETHER_HANDOFF_COST_NS",
    doc: "Pins the cross-worker handoff-cost estimate (nanoseconds) and freezes \
          live refinement — deterministic tests / a known-good number on a noisy \
          box. Unset → boot-probed and live-refined.",
    default: None,
    kind: KnobKind::HandRegistered,
}];

/// Round trips measured (after warmup). Even, so the median averages the
/// two central samples; large enough to median out scheduler jitter,
/// small enough that the whole probe is sub-millisecond.
const TRIALS: usize = 64;

/// Round trips discarded before sampling, to let both threads' caches and
/// the CPU's frequency scaling settle so the measured edges reflect the
/// steady-state handoff rather than a cold first wake.
const WARMUP: usize = 16;

/// Constant EWMA shift `k`: `mean += (x − mean) >> k`. `k = 4` is α = 1/16,
/// matching `aether_actor::cost::EWMA_SHIFT` so the handoff estimate
/// smooths over the same ~16-sample window the per-handler cost cell does.
/// Power-of-two so the live fold is a shift, not a float multiply.
const EWMA_SHIFT: u32 = 4;

/// Process-global cross-worker handoff cost estimate (nanos): seeded by the
/// boot probe ([`HandoffEwma::seed`]) and refined by live `notify → wake`
/// samples ([`HandoffEwma::fold`]).
///
/// Unlike `aether_actor::cost::CostCell` — which a single actor folds under
/// the actor lock, so a plain `load → compute → store` suffices — this cell
/// is folded by **many woken workers concurrently** (each folds its own
/// wake latency on resume), so the fold is a `compare_exchange` loop to
/// avoid lost updates. Contention is low: a fold happens only on a genuine
/// idle → busy wake (the route-to-spinner fast path folds nothing), so the
/// CAS almost never spins.
struct HandoffEwma {
    /// EWMA of the per-wake `notify → wake` latency, nanos.
    mean: AtomicU64,
    /// Folded-sample count (boot seed counts as 1). Diagnostic / test
    /// observability; never gates a decision.
    samples: AtomicU64,
}

impl HandoffEwma {
    const fn new() -> Self {
        Self {
            mean: AtomicU64::new(0),
            samples: AtomicU64::new(0),
        }
    }

    /// Seed the estimate from the boot probe (or the env override). Sets
    /// the mean directly so the estimate is meaningful immediately, and
    /// marks the cell seeded (`samples = 1`) so the first live fold updates
    /// rather than ramps from zero. Called once, before any live fold (the
    /// [`ensure_seeded`] `OnceLock` gates ordering).
    fn seed(&self, nanos: u64) {
        self.mean.store(nanos, Ordering::Relaxed);
        self.samples.store(1, Ordering::Relaxed);
    }

    /// Fold one live `notify → wake` sample (nanos). CAS-serialized so
    /// concurrent woken workers don't lose updates; the sample count uses
    /// a `fetch_add` for the same reason.
    fn fold(&self, sample: u64) {
        let mut mean = self.mean.load(Ordering::Relaxed);
        loop {
            let next = ewma_step(mean, sample, EWMA_SHIFT);
            match self
                .mean
                .compare_exchange_weak(mean, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => mean = observed,
            }
        }
        self.samples.fetch_add(1, Ordering::Relaxed);
    }

    fn mean(&self) -> u64 {
        self.mean.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn samples(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }
}

/// The process-global handoff estimate. Const-constructed (atomics start
/// at 0); seeded lazily on first access via [`ensure_seeded`].
static HANDOFF: HandoffEwma = HandoffEwma::new();

/// Guards the one-time boot seed of [`HANDOFF`].
static SEEDED: OnceLock<()> = OnceLock::new();

/// Set when the estimate is pinned via `AETHER_HANDOFF_COST_NS`; live folds
/// are skipped so the pinned value can't drift.
static PINNED: AtomicBool = AtomicBool::new(false);

/// Seed [`HANDOFF`] exactly once: from `AETHER_HANDOFF_COST_NS` if set
/// (and freeze live refinement), else from the boot probe.
fn ensure_seeded() {
    SEEDED.get_or_init(
        || match parse_cost_override(env::var("AETHER_HANDOFF_COST_NS").ok()) {
            Some(pinned) => {
                HANDOFF.seed(pinned);
                PINNED.store(true, Ordering::Relaxed);
            }
            None => HANDOFF.seed(measure_handoff_cost_nanos()),
        },
    );
}

/// The calibrated cross-worker handoff cost for this process: the boot
/// probe's seed, refined by live `notify → wake` samples. The keep-local
/// valve and iamacoffeepot/aether#1127's recruiter read this as the
/// box-relative "what does a handoff cost here" reference. Floored to 1ns.
#[must_use]
pub fn handoff_cost() -> Duration {
    ensure_seeded();
    Duration::from_nanos(HANDOFF.mean().max(1))
}

/// [`handoff_cost`] as a flat `u64` of nanoseconds. Both nanos consumers
/// — the boot-time keep-local calibration log and the recruit-K wake gate
/// — read this so the `Duration → nanos` conversion lives in one place;
/// the 1 ns floor stays in [`handoff_cost`]. Saturates to `u64::MAX` on
/// overflow, which is unreachable: the estimate is a `u64` of nanos by
/// construction, so the `Duration` can never exceed `u64::MAX` nanos.
#[must_use]
pub fn handoff_cost_nanos() -> u64 {
    u64::try_from(handoff_cost().as_nanos()).unwrap_or(u64::MAX)
}

/// Fold one live `notify → wake` latency (nanos) into the estimate —
/// called from [`super::spin_park`] each time a parked worker resumes from
/// a producer's `unpark`. A no-op when the estimate is pinned. Dark: drives
/// nothing. Floored to 1ns (a handoff is never truly free; a 0 from clock
/// granularity would only drag the EWMA down).
pub fn fold_handoff_sample(nanos: u64) {
    ensure_seeded();
    if PINNED.load(Ordering::Relaxed) {
        return;
    }
    HANDOFF.fold(nanos.max(1));
}

/// Folded-sample count of the live estimate (boot seed counts as 1).
/// Test / diagnostic observability for the dark refinement path.
#[cfg(test)]
pub fn handoff_samples() -> u64 {
    ensure_seeded();
    HANDOFF.samples()
}

/// Parse the `AETHER_HANDOFF_COST_NS` override: a positive integer
/// nanosecond count, else `None` (unset / unparseable / `< 1` all fall
/// back to the live probe). Split out from the env read so the parse is
/// unit-testable without mutating process env.
fn parse_cost_override(raw: Option<String>) -> Option<u64> {
    raw.and_then(|v| v.parse::<u64>().ok()).filter(|&n| n >= 1)
}

/// Log the calibrated handoff cost and the keep-local time budget now
/// derived from it ([`super::worker_deque::time_budget`]), plus their ratio
/// — the *realized* multiplier, which equals `BUDGET_HANDOFF_MULTIPLIER`
/// unless a clamp rail or an `AETHER_LOCAL_TIME_BUDGET_US` override bound
/// it. The ratio is the cross-box validation signal: read it out of
/// `actor_logs` on any box (especially the CI VM) to confirm the budget
/// landed where intended. Called once at chassis boot.
pub fn log_handoff_calibration() {
    let cost_nanos = handoff_cost_nanos();
    let budget_nanos =
        u64::try_from(super::worker_deque::time_budget().as_nanos()).unwrap_or(u64::MAX);
    // Realized handoffs-per-budget on this box: `BUDGET_HANDOFF_MULTIPLIER`
    // in the common case, or off it when a rail / env override bound the
    // budget — a one-glance check that the wiring landed as intended.
    #[allow(
        clippy::cast_precision_loss,
        reason = "nanosecond counts are well within f64's exact-integer range; the ratio is a diagnostic, not load-bearing"
    )]
    let realized_multiplier = budget_nanos as f64 / cost_nanos as f64;
    tracing::info!(
        target: "aether_substrate::scheduler",
        handoff_cost_nanos = cost_nanos,
        budget_nanos,
        realized_multiplier,
        "keep-local time budget derived from the measured cross-worker handoff cost (iamacoffeepot/aether#1182)",
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
// Boot-time scheduler calibration probe — measures handoff cost before any actor
// runs; no mail, no ctx, no settlement chain. (Spawn is a block-tail expression, so
// the allow sits on the fn rather than the statement.)
#[allow(clippy::disallowed_methods)]
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
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
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
    fn handoff_cost_nanos_matches_duration_view() {
        let nanos = handoff_cost_nanos();
        assert_eq!(
            nanos,
            u64::try_from(handoff_cost().as_nanos())
                .expect("handoff_cost nanos fit u64 by construction")
        );
        assert!(
            nanos >= 1,
            "the 1 ns floor in handoff_cost must carry through"
        );
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

    #[test]
    fn handoff_ewma_seed_then_live_samples_track() {
        // Boot seeds the estimate; live samples pull it toward the
        // operating cost. The seed is reported immediately (no ramp from
        // zero), then a sustained higher latency converges the mean within
        // the shift granularity of the new level.
        let granularity = 1u64 << EWMA_SHIFT;
        let cell = HandoffEwma::new();
        cell.seed(2_000);
        assert_eq!(cell.mean(), 2_000, "seed is reported directly");
        assert_eq!(cell.samples(), 1, "boot seed counts as one sample");

        for _ in 0..200 {
            cell.fold(5_000);
        }
        assert!(
            5_000 - cell.mean() < granularity,
            "live folds converge toward the operating cost: {}",
            cell.mean(),
        );
        assert_eq!(cell.samples(), 201, "every live fold counted");
    }

    /// Contention/backoff-sensitive tests live in `mod heavy`: they exercise
    /// the concurrent handoff-EWMA fold path, so they are serialized into the
    /// `serial-heavy` nextest group (`.config/nextest.toml`) to avoid
    /// oversubscribing cores against one another.
    mod heavy {
        use super::*;

        /// Many workers fold the same cell concurrently (the real wake
        /// pattern): the CAS fold + `fetch_add` count must lose no update.
        /// Folding the seed value keeps the mean fixed, so the final mean and
        /// the exact sample count are both deterministic regardless of
        /// interleaving — a lost CAS or a lost increment would show up.
        #[test]
        fn handoff_ewma_concurrent_folds_lose_nothing() {
            const THREADS: usize = 8;
            const PER_THREAD: u64 = 2_000;
            let cell = Arc::new(HandoffEwma::new());
            cell.seed(1_000);

            let workers: Vec<_> = (0..THREADS)
                .map(|_| {
                    let cell = Arc::clone(&cell);
                    thread::spawn(move || {
                        for _ in 0..PER_THREAD {
                            cell.fold(1_000);
                        }
                    })
                })
                .collect();
            for w in workers {
                w.join().expect("fold worker panicked");
            }

            assert_eq!(
                cell.mean(),
                1_000,
                "folding the seed value leaves the mean fixed under any interleaving",
            );
            assert_eq!(
                cell.samples(),
                1 + THREADS as u64 * PER_THREAD,
                "every concurrent fold's count survives (no lost fetch_add)",
            );
        }
    }
}
