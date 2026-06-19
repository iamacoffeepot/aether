// iamacoffeepot/aether#1128 global per-handler execution-cost table —
// Phase 0 of iamacoffeepot/aether#1127's cost-aware recruiter.
// **Measure-only; no scheduling change.**
//
// The cold-path index over the per-handler [`CostCell`]s (defined in this
// module). Cost is *measured* at the recipient (its
// handler runs, and the dispatch fold writes the cell through that
// actor's lock-free per-actor `CostCells` cache — on whichever worker is
// dispatching it, exclusively) but *consumed* cross-thread — by the
// `cost.tail` dump here, and by a future iamacoffeepot/aether#1178
// producer-side `Σw` / `w_max` read at flush. Both reach the *same*
// `Arc<CostCell>` through this global table.
//
// Mirrors the routing-sibling [`CapabilityRegistry`] (`capability.rs`):
// `RwLock<HashMap<_, _>>` hung off the [`Mailer`](super::mailer::Mailer),
// seeded/torn-down when an actor is constructed / replaced / dropped
// (rare) and read on the cold dump — never on the per-dispatch fold
// (that runs lock-free through the per-actor cache). Keyed by
// `(MailboxId, KindId)` so one mailbox's handler set is
// contiguous-by-filter and the recruiter can sum a recipient group.

// The table's `RwLock` guard is held across the
// resolve-then-row-build pair in `tail` — the same low-contention
// rationale as the routing registry's guard policy.
#![allow(clippy::significant_drop_tightening)]

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard};

use core::sync::atomic::{AtomicU64, Ordering};

use aether_actor::Local;
use aether_kinds::{CostRow, CostTail, CostTailResult};

use crate::mail::{KindId, MailboxId};

/// Constant EWMA shift `k`: `mean += (x − mean) >> k`. `k = 4` is an α of
/// `1/16` — recent samples weigh ~6% each, so the estimate tracks a
/// sustained shift within ~16 dispatches while smoothing one-off outliers.
/// Power-of-two so the update is a shift, not a float multiply — no float on
/// the dispatch hot path.
pub const EWMA_SHIFT: u32 = 4;

/// One handler's execution-cost EWMA in fixed-point nanos
/// (iamacoffeepot/aether#1128). The fold is single-writer-serialized by the
/// actor lock (an actor dispatches on one thread at a time), so the RMW is a
/// plain `load → compute → store` rather than a CAS loop; cross-thread
/// readers (the `cost.tail` dump, the future #1178 recruiter) see an
/// eventually-consistent estimate. `Relaxed` throughout — no ordering is
/// needed between the three cells, and it is zero-cost over a hypothetical
/// plain `u64` on the target ISAs.
#[derive(Debug, Default)]
pub struct CostCell {
    /// EWMA of the per-dispatch handler execution time, nanos.
    mean: AtomicU64,
    /// EWMA of the absolute deviation `|x − mean|`, nanos — a cheap spread
    /// signal (mean absolute deviation, not variance) so a reader can tell a
    /// steady handler from a bimodal one.
    mad: AtomicU64,
    /// Count of folded samples. `0` is the neutral seed: a handler that is
    /// *known* (pre-seeded from the load-time handler set) but has not run
    /// yet. Distinguishes "known-but-unrun" from "absent".
    samples: AtomicU64,
}

impl CostCell {
    /// A fresh neutral-seed cell: `mean = mad = samples = 0`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one execution-time `sample` (nanos) into the EWMA. The first
    /// sample seeds `mean` directly (no warm-up bias); subsequent samples
    /// apply the constant-α update. Plain `load → compute → store` —
    /// single-writer per the actor lock, so no CAS.
    pub fn fold(&self, sample: u64) {
        let prior = self.samples.load(Ordering::Relaxed);
        if prior == 0 {
            self.mean.store(sample, Ordering::Relaxed);
            self.mad.store(0, Ordering::Relaxed);
        } else {
            let mean = self.mean.load(Ordering::Relaxed);
            let next_mean = ewma_step(mean, sample, EWMA_SHIFT);
            self.mean.store(next_mean, Ordering::Relaxed);

            let dev = sample.abs_diff(next_mean);
            let mad = self.mad.load(Ordering::Relaxed);
            self.mad
                .store(ewma_step(mad, dev, EWMA_SHIFT), Ordering::Relaxed);
        }
        self.samples
            .store(prior.saturating_add(1), Ordering::Relaxed);
    }

    /// Current EWMA mean (nanos). `0` before the first fold.
    #[must_use]
    pub fn mean_nanos(&self) -> u64 {
        self.mean.load(Ordering::Relaxed)
    }

    /// Current EWMA mean-absolute-deviation (nanos). `0` before the second
    /// fold.
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

/// One constant-α EWMA step toward `sample`, computed on the signed
/// difference (`current + (sample − current) >> shift`) so a falling cost
/// converges as fast as a rising one. Integer-only. Shared with the
/// scheduler's handoff calibration, which folds with its own shift.
#[must_use]
pub fn ewma_step(current: u64, sample: u64, shift: u32) -> u64 {
    if sample >= current {
        current + ((sample - current) >> shift)
    } else {
        current - ((current - sample) >> shift)
    }
}

/// Per-actor cache mapping a handled `KindId` to its shared [`CostCell`].
/// Stamped on the actor's `ActorSlots` as a [`Local`]; the dispatch fold
/// reaches it lock-free on the actor's own thread. Each `Arc<CostCell>` is
/// shared with the global [`CostTable`] so the cold dump and the recruiter
/// read the same cell the fold writes.
///
/// A `Vec` rather than a map: handler sets are tiny (a handful of kinds per
/// actor), so a linear scan beats a hash.
#[derive(Debug, Default)]
pub struct CostCells(Vec<(KindId, Arc<CostCell>)>);

impl Local for CostCells {}

impl CostCells {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Install the actor's handler-kind cells, sharing each `Arc<CostCell>`
    /// with the global table. Called once at actor construction (under the
    /// dispatch stamp); a `replace`-spawned actor re-seeds wholesale and drop
    /// seeds an empty `Vec` to clear.
    pub fn seed(&mut self, cells: Vec<(KindId, Arc<CostCell>)>) {
        self.0 = cells;
    }

    /// Look up the cell for `kind`. `None` for a kind not in the handler set
    /// (framework arms like `log.tail`, or a fallback dispatch) — the fold
    /// skips those.
    #[must_use]
    pub fn get(&self, kind: KindId) -> Option<&Arc<CostCell>> {
        self.0.iter().find(|(k, _)| *k == kind).map(|(_, c)| c)
    }

    /// Borrow the `(kind, cell)` pairs for the dump. Read-only.
    #[must_use]
    pub fn entries(&self) -> &[(KindId, Arc<CostCell>)] {
        &self.0
    }
}

/// A single handler's measured cost, resolved from a [`CostCell`] under
/// one read-lock. Carries the EWMA mean plus the two confidence signals
/// the iamacoffeepot/aether#1178 recruiter gates on (`samples == 0` ⇒
/// neutral seed; a high `mad_nanos` ⇒ bimodal / untrustworthy), so the
/// caller never re-touches the cell after the batch read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostSample {
    /// EWMA mean execution time (nanos). `0` before the first fold.
    pub mean_nanos: u64,
    /// Folded-sample count. `0` is the neutral seed — known handler,
    /// never run, so the mean is meaningless to the recruiter.
    pub samples: u64,
    /// EWMA mean-absolute-deviation (nanos) — the spread signal.
    pub mad_nanos: u64,
}

/// Substrate-owned global index over every actor's per-handler
/// [`CostCell`]s. Shared as part of the [`Mailer`](super::mailer::Mailer)
/// (mirroring how the routing [`Registry`](super::registry::Registry)
/// and [`CapabilityRegistry`](super::capability::CapabilityRegistry) are
/// shared). The load / replace / drop hooks `seed` / `drop_mailbox`; the
/// cold `cost.tail` dump and a future producer-side recruiter read `tail`
/// / `cells_for`.
#[derive(Debug, Default)]
pub struct CostTable {
    cells: RwLock<HashMap<(MailboxId, KindId), Arc<CostCell>>>,
}

impl CostTable {
    /// A fresh, empty table. The boot path builds one and shares it via
    /// the [`Mailer`](super::mailer::Mailer).
    #[must_use]
    pub fn new() -> Self {
        Self {
            cells: RwLock::new(HashMap::new()),
        }
    }

    /// Seed a neutral cell (`samples = 0`) for every kind in `kinds`
    /// under `mailbox`, returning the `(kind, Arc<CostCell>)` pairs so
    /// the caller can stamp the *same* `Arc`s into the actor's
    /// per-actor `CostCells` cache. Re-seeding an existing
    /// `(mailbox, kind)` reuses the prior cell (so a `replace` against
    /// the same handler set keeps its accumulated estimate); a fresh
    /// kind gets a new neutral cell. Called at actor construction
    /// (`WasmTrampoline::init` / the native-cap boot wrap, both under
    /// `with_stamped`) and on replace, paired with a `CostCells` cache
    /// stamp of the returned `Arc`s so both indexes share the cell.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned — a poisoned lock means a
    /// prior writer panicked mid-update, a substrate-level invariant
    /// violation (fail-fast per ADR-0063).
    pub fn seed(&self, mailbox: MailboxId, kinds: &[KindId]) -> Vec<(KindId, Arc<CostCell>)> {
        let mut guard = self.cells.write().expect("cost table lock poisoned");
        kinds
            .iter()
            .map(|&kind| {
                let cell = guard
                    .entry((mailbox, kind))
                    .or_insert_with(|| Arc::new(CostCell::new()));
                (kind, Arc::clone(cell))
            })
            .collect()
    }

    /// Remove every cell for `mailbox`. Called on
    /// `aether.component.drop` / unload. A no-op for an unknown mailbox.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned (see [`Self::seed`]).
    pub fn drop_mailbox(&self, mailbox: MailboxId) {
        let mut guard = self.cells.write().expect("cost table lock poisoned");
        guard.retain(|(m, _), _| *m != mailbox);
    }

    /// The shared `Arc<CostCell>`s for one `mailbox`, as `(kind, cell)`
    /// pairs — the cross-actor read the global index exists for. A future
    /// iamacoffeepot/aether#1178 recruiter sums these per recipient group
    /// at the producer's flush (the per-actor caches are private to each
    /// recipient's dispatch; this table is how a producer reads them).
    /// Cold path — read lock.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned (see [`Self::seed`]).
    #[must_use]
    pub fn cells_for(&self, mailbox: MailboxId) -> Vec<(KindId, Arc<CostCell>)> {
        let guard = self.cells.read().expect("cost table lock poisoned");
        guard
            .iter()
            .filter(|((m, _), _)| *m == mailbox)
            .map(|((_, k), c)| (*k, Arc::clone(c)))
            .collect()
    }

    /// Acquire one read-lock over the table and hand back a
    /// [`CostLookup`] that resolves `(MailboxId, KindId)` point lookups
    /// for the duration of a single flush — iamacoffeepot/aether#1178's
    /// read side of iamacoffeepot/aether#1128's table. The recruiter
    /// holds the returned guard across its whole group accumulation pass
    /// so each mail's cost resolves under the *same* lock acquire, rather
    /// than a `read()` per mail on the hot flush path (the rejected
    /// per-mail point lookup). The lock is a low-contention `RwLock`
    /// (writers are the rare seed / drop hooks), so holding the read-lock
    /// for one producer's flush adds no measurable contention — the same
    /// guard policy [`Self::tail`] uses.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned (see [`Self::seed`]).
    #[must_use]
    pub fn lookup(&self) -> CostLookup<'_> {
        CostLookup {
            cells: self.cells.read().expect("cost table lock poisoned"),
        }
    }

    /// Dump `mailbox`'s cost rows, filtered to `request.kind` when set.
    /// `kind_name` is left `None` here — the table holds ids, not names;
    /// the `cost.tail` dispatch arm (or the MCP layer) resolves names
    /// against the registry on the cold render path. Cold path — read
    /// lock.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned (see [`Self::seed`]).
    #[must_use]
    pub fn tail(&self, mailbox: MailboxId, request: &CostTail) -> CostTailResult {
        let guard = self.cells.read().expect("cost table lock poisoned");
        let rows = guard
            .iter()
            .filter(|((m, _), _)| *m == mailbox)
            .filter(|((_, k), _)| request.kind.is_none_or(|want| *k == want))
            .map(|((_, k), c)| CostRow {
                kind_id: *k,
                kind_name: None,
                mean_nanos: c.mean_nanos(),
                mad_nanos: c.mad_nanos(),
                samples: c.samples(),
            })
            .collect();
        CostTailResult::Ok { rows }
    }
}

/// A read-lock held over a [`CostTable`] for the span of one flush, plus
/// the point-lookup the recruiter resolves each mail's cost through.
/// Acquired once via [`CostTable::lookup`] and dropped when the flush's
/// group accumulation finishes — so the whole `Σw` / `w_max` pass runs
/// under a single read acquire, not one per mail.
#[derive(Debug)]
pub struct CostLookup<'a> {
    cells: RwLockReadGuard<'a, HashMap<(MailboxId, KindId), Arc<CostCell>>>,
}

impl CostLookup<'_> {
    /// Resolve the measured cost of one `(mailbox, kind)` handler, or
    /// `None` for a `(mailbox, kind)` the table has never seeded (the
    /// handler is absent — distinct from a *seeded-but-unrun* cell, which
    /// resolves to `Some` with `samples == 0`). The caller treats `None`
    /// and `samples == 0` alike — both are "unknown cost" — but the
    /// distinction is preserved for the dump / future callers.
    #[must_use]
    pub fn get(&self, mailbox: MailboxId, kind: KindId) -> Option<CostSample> {
        self.cells.get(&(mailbox, kind)).map(|cell| CostSample {
            mean_nanos: cell.mean_nanos(),
            samples: cell.samples(),
            mad_nanos: cell.mad_nanos(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn first_sample_seeds_mean_directly() {
        let cell = CostCell::new();
        cell.fold(5_000);
        assert_eq!(cell.mean_nanos(), 5_000);
        assert_eq!(cell.samples(), 1);
    }

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

    #[test]
    fn neutral_seed_reports_zero_samples() {
        let cell = CostCell::new();
        assert_eq!(cell.samples(), 0);
        assert_eq!(cell.mean_nanos(), 0);
        assert_eq!(cell.mad_nanos(), 0);
    }

    #[test]
    fn cost_cells_seed_and_lookup() {
        let mut cells = CostCells::new();
        assert!(cells.get(KindId(10)).is_none(), "empty cache misses");
        let a = Arc::new(CostCell::new());
        let b = Arc::new(CostCell::new());
        cells.seed(vec![
            (KindId(10), Arc::clone(&a)),
            (KindId(20), Arc::clone(&b)),
        ]);
        assert!(cells.get(KindId(10)).is_some());
        assert!(cells.get(KindId(20)).is_some());
        assert!(cells.get(KindId(30)).is_none());
        // The cached Arc and the seeded Arc share the same cell.
        cells.get(KindId(10)).expect("kind 10 is seeded").fold(777);
        assert_eq!(a.mean_nanos(), 777);
    }

    #[test]
    fn seed_returns_neutral_cells_in_both_indexes() {
        let table = CostTable::new();
        let mbx = MailboxId(7);
        let handed = table.seed(mbx, &[KindId(10), KindId(20)]);
        assert_eq!(handed.len(), 2);
        // The handed-back Arcs are the table's own cells (shared index):
        // a fold through the handed Arc is visible through `tail`.
        for (_, cell) in &handed {
            assert_eq!(cell.samples(), 0, "neutral seed");
        }

        let CostTailResult::Ok { rows } = table.tail(mbx, &CostTail { kind: None }) else {
            panic!("expected Ok");
        };
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.samples == 0));
    }

    #[test]
    fn fold_through_handed_arc_is_visible_in_tail() {
        let table = CostTable::new();
        let mbx = MailboxId(7);
        let handed = table.seed(mbx, &[KindId(10)]);
        handed[0].1.fold(2_000);

        let CostTailResult::Ok { rows } = table.tail(
            mbx,
            &CostTail {
                kind: Some(KindId(10)),
            },
        ) else {
            panic!("expected Ok");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind_id, KindId(10));
        assert_eq!(rows[0].mean_nanos, 2_000);
        assert_eq!(rows[0].samples, 1);
    }

    #[test]
    fn re_seed_reuses_prior_cell() {
        let table = CostTable::new();
        let mbx = MailboxId(7);
        let first = table.seed(mbx, &[KindId(10)]);
        first[0].1.fold(5_000);
        // Replace against the same handler set: the cell (and its
        // accumulated estimate) is reused.
        let second = table.seed(mbx, &[KindId(10)]);
        assert_eq!(second[0].1.mean_nanos(), 5_000);
        assert_eq!(second[0].1.samples(), 1);
    }

    #[test]
    fn tail_kind_filter_narrows_rows() {
        let table = CostTable::new();
        let mbx = MailboxId(7);
        table.seed(mbx, &[KindId(10), KindId(20)]);
        let CostTailResult::Ok { rows } = table.tail(
            mbx,
            &CostTail {
                kind: Some(KindId(10)),
            },
        ) else {
            panic!("expected Ok");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind_id, KindId(10));
    }

    #[test]
    fn drop_mailbox_clears_only_that_mailbox() {
        let table = CostTable::new();
        let a = MailboxId(7);
        let b = MailboxId(8);
        table.seed(a, &[KindId(10)]);
        table.seed(b, &[KindId(10)]);
        table.drop_mailbox(a);

        let CostTailResult::Ok { rows } = table.tail(a, &CostTail { kind: None }) else {
            panic!("expected Ok");
        };
        assert!(rows.is_empty(), "dropped mailbox's cells gone");
        let CostTailResult::Ok { rows } = table.tail(b, &CostTail { kind: None }) else {
            panic!("expected Ok");
        };
        assert_eq!(rows.len(), 1, "sibling mailbox's cells survive");
    }

    #[test]
    fn cells_for_returns_mailbox_slice() {
        let table = CostTable::new();
        let mbx = MailboxId(7);
        table.seed(mbx, &[KindId(10), KindId(20)]);
        let cells = table.cells_for(mbx);
        assert_eq!(cells.len(), 2);
        assert!(cells.iter().any(|(k, _)| *k == KindId(10)));
        assert!(cells.iter().any(|(k, _)| *k == KindId(20)));
    }

    /// The batch [`CostTable::lookup`] resolves a seeded handler's folded
    /// mean under one read-lock; an unseeded `(mailbox, kind)` resolves to
    /// `None` and a seeded-but-unrun cell resolves to `Some` with
    /// `samples == 0` (the neutral-seed distinction the recruiter gates on).
    #[test]
    fn batch_lookup_resolves_means_and_misses() {
        let table = CostTable::new();
        let mbx = MailboxId(7);
        let handed = table.seed(mbx, &[KindId(10), KindId(20)]);
        // Fold a sample into kind 10 only; kind 20 stays a neutral seed.
        for (kind, cell) in &handed {
            if *kind == KindId(10) {
                cell.fold(3_000);
            }
        }

        let lookup = table.lookup();
        let ten = lookup.get(mbx, KindId(10)).expect("kind 10 seeded");
        assert_eq!(ten.mean_nanos, 3_000);
        assert_eq!(ten.samples, 1);

        let twenty = lookup.get(mbx, KindId(20)).expect("kind 20 seeded");
        assert_eq!(twenty.mean_nanos, 0, "neutral seed has no mean");
        assert_eq!(twenty.samples, 0, "neutral seed reports zero samples");

        assert!(
            lookup.get(mbx, KindId(30)).is_none(),
            "an unseeded handler resolves to None"
        );
        assert!(
            lookup.get(MailboxId(8), KindId(10)).is_none(),
            "a different mailbox's same kind id misses"
        );
    }

    /// Two mails routed to the *same* handler resolve the same cost under
    /// one batch read — the per-mail point lookup the recruiter sums into a
    /// group's `Σw`, without a lock acquire per mail.
    #[test]
    fn batch_lookup_repeats_under_one_lock() {
        let table = CostTable::new();
        let mbx = MailboxId(7);
        let handed = table.seed(mbx, &[KindId(10)]);
        handed[0].1.fold(5_000);

        let lookup = table.lookup();
        let first = lookup.get(mbx, KindId(10)).expect("seeded");
        let second = lookup.get(mbx, KindId(10)).expect("seeded");
        assert_eq!(first, second);
        assert_eq!(first.mean_nanos, 5_000);
    }
}
