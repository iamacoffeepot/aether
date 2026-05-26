//! iamacoffeepot/aether#1128 per-handler execution-cost EWMA — Phase 0
//! of iamacoffeepot/aether#1127's cost-aware recruiter. **Measure-only
//! dark instrumentation; no scheduling change.**
//!
//! Each actor folds `(Finished.t − Received.t)` from the existing
//! [`crate::trace_ring`] dispatch bracket into a per-handler
//! [`CostCell`] — a constant-α EWMA of the handler's execution time in
//! fixed-point nanos. The fold runs inside the dispatch
//! `local::with_stamped` block, so it reaches its cell through the
//! per-actor [`CostCells`] [`Local`] cache — a lock-free lookup, the hot
//! path. An actor dispatches on one worker at a time (the `Ready→Running`
//! exclusivity), so the cell has a single writer at a time even though
//! the worker varies across cycles — which is why the cells are atomic
//! but the fold needs no CAS.
//!
//! The *same* `Arc<CostCell>` lives in a second index — a global
//! `RwLock<HashMap<(MailboxId, KindId), Arc<CostCell>>>` hung off the
//! substrate's `Mailer` (`aether-substrate`'s `cost::CostTable`) — read
//! only on cold paths (the `cost.tail` dump, and a future
//! iamacoffeepot/aether#1178 producer-side `Σw` read at flush). Cost is
//! *measured* at the recipient but *consumed* cross-thread by the
//! producer, so private per-actor storage would be unreadable by the
//! recruiter; the shared `Arc` keeps the per-actor cache *and* the
//! global index over one allocation.
//!
//! **No `unsafe`, no lock on the fold.** Soundness lives in the type:
//! [`core::sync::atomic::AtomicU64`] makes every load/store race-free
//! for any access pattern (the producer reads a cell while the recipient
//! folds it — a writer↔reader race atomics are required to make
//! defined), and `Relaxed` lowers to a plain `mov`/`ldr` on the target
//! ISAs. The actor-lock single-writer invariant (an actor runs on one
//! dispatch thread at a time) buys **accuracy, not soundness**: it lets
//! the fold be a plain `load → compute → store` instead of a
//! `compare_exchange` CAS loop, because no second thread folds the same
//! cell. Cross-thread readers tolerate a slightly stale estimate.
//!
//! Mirrors the ADR-0081 [`crate::log::ActorLogRing`] mechanism: a
//! [`Local`]-implementing newtype stamped on the actor's `ActorSlots`,
//! reached via `try_with` / `try_with_mut` from inside the dispatch
//! `with_stamped` block.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use aether_data::KindId;

use crate::Local;

/// Constant EWMA shift `k`: `mean += (x − mean) >> k`. `k = 4` is an
/// α of `1/16` — recent samples weigh ~6% each, so the estimate tracks
/// a sustained shift within ~16 dispatches while smoothing one-off
/// outliers. Power-of-two so the update is a shift, not a float
/// multiply — no float on the dispatch hot path.
pub const EWMA_SHIFT: u32 = 4;

/// One handler's execution-cost EWMA in fixed-point nanos. The fold is
/// single-writer-serialized by the actor lock (an actor dispatches on
/// one thread at a time), so the RMW is a plain `load → compute →
/// store` rather than a CAS loop; cross-thread readers (the `cost.tail`
/// dump, the future iamacoffeepot/aether#1178 recruiter) see an
/// eventually-consistent estimate. `Relaxed` throughout — no ordering
/// is needed between the three cells (a reader tolerating a torn
/// mean/mad pair is harmless for a diagnostic estimate) and `Relaxed`
/// is zero-cost over a hypothetical plain `u64` on the target ISAs.
#[derive(Debug, Default)]
pub struct CostCell {
    /// EWMA of the per-dispatch handler execution time, nanos.
    mean: AtomicU64,
    /// EWMA of the absolute deviation `|x − mean|`, nanos — a cheap
    /// spread signal (mean absolute deviation, not variance) so a
    /// reader can tell a steady handler from a bimodal one.
    mad: AtomicU64,
    /// Count of folded samples. `0` is the neutral seed: a handler that
    /// is *known* (its cell was pre-seeded from the load-time handler
    /// set) but has not run yet. Distinguishes "known-but-unrun" from
    /// "absent" for the recruiter and the dump.
    samples: AtomicU64,
}

impl CostCell {
    /// A fresh neutral-seed cell: `mean = mad = samples = 0`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one execution-time `sample` (nanos) into the EWMA. The
    /// first sample seeds `mean` directly (no warm-up bias from
    /// shifting toward zero); subsequent samples apply the constant-α
    /// update. Plain `load → compute → store` — single-writer per the
    /// actor lock, so no CAS.
    pub fn fold(&self, sample: u64) {
        let prior = self.samples.load(Ordering::Relaxed);
        if prior == 0 {
            // Seed the EWMA from the first observation so the estimate
            // is meaningful immediately rather than ramping from zero.
            self.mean.store(sample, Ordering::Relaxed);
            self.mad.store(0, Ordering::Relaxed);
        } else {
            let mean = self.mean.load(Ordering::Relaxed);
            // Signed delta folded with a symmetric shift so the EWMA
            // tracks both rising and falling costs without float.
            let next_mean = ewma_step(mean, sample);
            self.mean.store(next_mean, Ordering::Relaxed);

            let dev = sample.abs_diff(next_mean);
            let mad = self.mad.load(Ordering::Relaxed);
            self.mad.store(ewma_step(mad, dev), Ordering::Relaxed);
        }
        self.samples
            .store(prior.saturating_add(1), Ordering::Relaxed);
    }

    /// Current EWMA mean (nanos). `0` before the first fold.
    #[must_use]
    pub fn mean_nanos(&self) -> u64 {
        self.mean.load(Ordering::Relaxed)
    }

    /// Current EWMA mean-absolute-deviation (nanos). `0` before the
    /// second fold.
    #[must_use]
    pub fn mad_nanos(&self) -> u64 {
        self.mad.load(Ordering::Relaxed)
    }

    /// Number of folded samples. `0` is the neutral seed.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }
}

/// One constant-α EWMA step toward `sample`: `current + (sample −
/// current) >> k`, computed on the signed difference so a falling cost
/// converges as fast as a rising one. Integer-only — the `>> k` is the
/// power-of-two α.
fn ewma_step(current: u64, sample: u64) -> u64 {
    if sample >= current {
        current + ((sample - current) >> EWMA_SHIFT)
    } else {
        current - ((current - sample) >> EWMA_SHIFT)
    }
}

/// Per-actor cache mapping a handled `KindId` to its shared
/// [`CostCell`]. Stamped on the actor's `ActorSlots` as a [`Local`],
/// exactly like the ADR-0081 [`crate::log::ActorLogRing`]; the fold and
/// the `cost.tail` dump both reach it via `try_with` / `try_with_mut`
/// from inside the dispatch `with_stamped` block on the actor's own
/// thread — a lock-free, single-threaded lookup.
///
/// A `Vec` rather than a map: handler sets are tiny (a handful of kinds
/// per actor), so a linear scan beats a hash and keeps `aether-actor`
/// free of the `hashbrown` dependency `no_std + alloc` would otherwise
/// pull (the same reasoning [`mod@crate::local`] uses for its `BTreeMap`).
#[derive(Debug, Default)]
pub struct CostCells(Vec<(KindId, Arc<CostCell>)>);

impl Local for CostCells {}

impl CostCells {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Install the actor's handler-kind cells, sharing each
    /// `Arc<CostCell>` with the global table. Called once at actor
    /// construction (`WasmTrampoline::init` / the native-cap boot wrap,
    /// both under `with_stamped`); a `replace`-spawned actor re-seeds
    /// wholesale against the post-replace handler set, and drop seeds an
    /// empty `Vec` to clear.
    pub fn seed(&mut self, cells: Vec<(KindId, Arc<CostCell>)>) {
        self.0 = cells;
    }

    /// Look up the cell for `kind`. `None` for a kind not in the
    /// handler set (framework arms like `log.tail` / `trace.tail`, or a
    /// fallback dispatch) — the fold skips those.
    #[must_use]
    pub fn get(&self, kind: KindId) -> Option<&Arc<CostCell>> {
        self.0.iter().find(|(k, _)| *k == kind).map(|(_, c)| c)
    }

    /// Borrow the `(kind, cell)` pairs for the dump. Read-only; the
    /// `cost.tail` framework arm reads the global table (filtered to
    /// the receiving mailbox) rather than this cache, but the pairs are
    /// surfaced for tests and a future in-actor dump.
    #[must_use]
    pub fn entries(&self) -> &[(KindId, Arc<CostCell>)] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A burst of identical samples converges the mean to that value
    /// (the first fold seeds it exactly; the rest are no-ops on a
    /// constant input).
    #[test]
    fn mean_converges_to_constant_input() {
        let cell = CostCell::new();
        for _ in 0..64 {
            cell.fold(1_000);
        }
        assert_eq!(cell.mean_nanos(), 1_000);
        assert_eq!(cell.mad_nanos(), 0, "no deviation on a constant input");
        assert_eq!(cell.samples(), 64);
    }

    /// The first fold seeds the mean directly — no warm-up ramp from
    /// zero — so a single sample reports its own value.
    #[test]
    fn first_sample_seeds_mean_directly() {
        let cell = CostCell::new();
        cell.fold(5_000);
        assert_eq!(cell.mean_nanos(), 5_000);
        assert_eq!(cell.samples(), 1);
    }

    /// Stepping the input up converges the EWMA toward the new level
    /// (monotonically, never overshooting). The integer `>> k` update
    /// stalls once the gap drops below `2^k` (the shift truncates to
    /// zero), so it lands *within* the shift granularity of the target,
    /// not exactly on it — the expected fixed-point behaviour.
    #[test]
    fn mean_tracks_a_step_up() {
        let granularity = 1u64 << EWMA_SHIFT;
        let cell = CostCell::new();
        cell.fold(100);
        assert_eq!(cell.mean_nanos(), 100);
        let mut last = cell.mean_nanos();
        for _ in 0..200 {
            cell.fold(10_000);
            let now = cell.mean_nanos();
            assert!(now >= last, "EWMA must not overshoot a step up");
            assert!(now <= 10_000, "EWMA must not exceed the input level");
            last = now;
        }
        assert!(
            10_000 - cell.mean_nanos() < granularity,
            "converges to within the shift granularity of the new level: {}",
            cell.mean_nanos()
        );
        assert!(cell.samples() > 1);
    }

    /// A falling input converges as fast as a rising one (symmetric
    /// shift), settling within the shift granularity of the lower level.
    #[test]
    fn mean_tracks_a_step_down() {
        let granularity = 1u64 << EWMA_SHIFT;
        let cell = CostCell::new();
        cell.fold(10_000);
        for _ in 0..200 {
            cell.fold(100);
        }
        assert!(
            cell.mean_nanos() - 100 < granularity,
            "converges to within the shift granularity of the lower level: {}",
            cell.mean_nanos()
        );
    }

    /// Deviation (MAD) rises above zero while the mean is mid-track on a
    /// step input — a steady handler shows ~0 MAD, a jumpy one shows a
    /// nonzero spread.
    #[test]
    fn mad_rises_on_a_step() {
        let cell = CostCell::new();
        cell.fold(100);
        for _ in 0..5 {
            cell.fold(10_000);
        }
        assert!(
            cell.mad_nanos() > 0,
            "MAD tracks the spread while the mean catches up"
        );
    }

    /// Neutral seed: a fresh cell (and one seeded into a [`CostCells`]
    /// cache but never folded) reports `samples = 0` — the
    /// known-but-unrun distinction.
    #[test]
    fn neutral_seed_reports_zero_samples() {
        let cell = CostCell::new();
        assert_eq!(cell.samples(), 0);
        assert_eq!(cell.mean_nanos(), 0);
        assert_eq!(cell.mad_nanos(), 0);
    }

    /// `CostCells` is empty until seeded; `seed` installs the pairs and
    /// `get` resolves a known kind while a stranger misses.
    #[test]
    fn cost_cells_seed_and_lookup() {
        let mut cells = CostCells::new();
        assert!(cells.get(KindId(10)).is_none(), "empty cache misses");
        let a = Arc::new(CostCell::new());
        let b = Arc::new(CostCell::new());
        cells.seed(alloc::vec![
            (KindId(10), Arc::clone(&a)),
            (KindId(20), Arc::clone(&b)),
        ]);
        assert!(cells.get(KindId(10)).is_some());
        assert!(cells.get(KindId(20)).is_some());
        assert!(cells.get(KindId(30)).is_none());
        // The cached Arc and the seeded Arc share the same cell: a fold
        // through one is visible through the other (the shared-index
        // invariant the global table relies on).
        cells.get(KindId(10)).expect("kind 10 is seeded").fold(777);
        assert_eq!(a.mean_nanos(), 777);
    }
}
