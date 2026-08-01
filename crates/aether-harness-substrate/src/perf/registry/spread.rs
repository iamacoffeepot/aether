//! Which routes each reader walks (iamacoffeepot/aether#4276).
//!
//! # The second layer
//!
//! Giving each reader its own kind ([`super::kinds::KindMix::PerReader`]) lifts
//! multi-reader throughput about fivefold, so the shared kind-name `Arc` is the
//! dominant serializer. It is not the only one: the per-reader arm still sits
//! *below* one reader's throughput and stays flat as readers are added, which is
//! the same signature at a smaller amplitude.
//!
//! The remaining candidate is the other thing `route_lookup` clones —
//! `endpoint.clone()`, two `Arc`s per lookup (the handler and the seize cell).
//! Those are per *route*, so they spread across the populated table instead of
//! landing on one line; but every reader walks the same routes, so every one of
//! those refcounts is still shared, just 256 ways instead of one.
//!
//! [`TargetSpread`] is that cut. `Overlapping` is the production shape — any
//! actor may address any mailbox, and hot routes are shared. `Disjoint` gives
//! each reader its own window of routes, so no two readers touch the same
//! endpoint refcount.
//!
//! # Why the windows are equal-length
//!
//! The obvious way to make readers disjoint — slice the table N ways — is
//! confounded, and badly. Each reader would walk `table / N` routes, so adding
//! readers would shrink every reader's working set; at 16 readers the slice fits
//! in L1 and the arm would post a large "improvement" that is cache residency,
//! not the absence of contention.
//!
//! So every reader walks exactly [`WALK_TARGETS`] routes under **both** spreads,
//! at every reader count. `Overlapping` points all of those windows at the same
//! routes and `Disjoint` points each at its own — the working set per reader is
//! identical and only the sharing differs. That is what forces the table to hold
//! `WALK_TARGETS × max readers` routes rather than the walk length itself, and
//! it is why reader 0's window is the same under both spreads: the single-reader
//! cell the two columns are each scaled against is one measurement, not two.

use aether_substrate::MailboxId;

/// Routes each reader walks per pass, under every spread and reader count.
///
/// Held constant so the two spreads differ only in *whose* routes they are. See
/// the module docs — a spread that varied this would measure cache residency.
pub const WALK_TARGETS: usize = 256;

/// Whether readers walk the same routes or their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetSpread {
    /// Every reader walks the same window, rotated so they do not step through
    /// it in lockstep. The production shape, and what every earlier reading of
    /// this sweep measured.
    Overlapping,
    /// Each reader walks a window of its own, so no two readers touch the same
    /// route's endpoint refcount.
    Disjoint,
}

impl TargetSpread {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Overlapping => "overlapping",
            Self::Disjoint => "disjoint",
        }
    }

    /// The [`WALK_TARGETS`]-long window reader `worker` walks, taken from the
    /// populated table `all`.
    ///
    /// Reader 0 gets the same window under both spreads, which is what makes the
    /// two columns' single-reader baselines the same measurement.
    #[must_use]
    pub fn walk(self, all: &[MailboxId], worker: usize) -> Vec<MailboxId> {
        match self {
            Self::Overlapping => {
                all.iter().copied().take(WALK_TARGETS).cycle().skip(worker).take(WALK_TARGETS).collect()
            }
            Self::Disjoint => all.iter().copied().skip(worker * WALK_TARGETS).take(WALK_TARGETS).collect(),
        }
    }

    /// Whether `all` holds enough routes to give `threads` readers the windows
    /// this spread needs. A caller that skips the cell rather than shortening a
    /// window is what keeps the walk length constant.
    #[must_use]
    pub fn covers(self, all: usize, threads: usize) -> bool {
        match self {
            Self::Overlapping => all >= WALK_TARGETS,
            Self::Disjoint => all >= WALK_TARGETS * threads,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn table(n: usize) -> Vec<MailboxId> {
        (0..n).map(|i| MailboxId(i as u64 + 1)).collect()
    }

    /// Tripwire (iamacoffeepot/aether#4276): every reader walks the same number
    /// of routes under both spreads and at every reader count.
    ///
    /// This is the property the whole comparison rests on. A `Disjoint` arm that
    /// sliced the table N ways would shrink each reader's working set as readers
    /// were added, and would post a cache-residency win as though it were the
    /// absence of contention — a wrong answer that looks like the expected one.
    #[test]
    fn every_reader_walks_the_same_number_of_routes_under_both_spreads() {
        let all = table(WALK_TARGETS * 16);
        for spread in [TargetSpread::Overlapping, TargetSpread::Disjoint] {
            for threads in [1usize, 2, 4, 8, 16] {
                for worker in 0..threads {
                    assert_eq!(
                        spread.walk(&all, worker).len(),
                        WALK_TARGETS,
                        "{} reader {worker} of {threads} walked a short window",
                        spread.label(),
                    );
                }
            }
        }
    }

    /// Tripwire: `Disjoint` readers genuinely share no route. Any overlap puts
    /// them back on a shared endpoint refcount, which is the thing the arm
    /// exists to remove.
    #[test]
    fn disjoint_readers_share_no_route() {
        let all = table(WALK_TARGETS * 16);
        let mut seen = HashSet::new();
        for worker in 0..16 {
            for target in TargetSpread::Disjoint.walk(&all, worker) {
                assert!(seen.insert(target), "route {target:?} appeared in two disjoint windows");
            }
        }
    }

    /// Tripwire: `Overlapping` readers genuinely share every route — it is the
    /// control the disjoint arm is read against, so a rotation that accidentally
    /// partitioned them would quietly turn the comparison into a null one.
    #[test]
    fn overlapping_readers_share_every_route() {
        let all = table(WALK_TARGETS * 16);
        let first: HashSet<_> = TargetSpread::Overlapping.walk(&all, 0).into_iter().collect();
        for worker in 1..16 {
            let other: HashSet<_> = TargetSpread::Overlapping.walk(&all, worker).into_iter().collect();
            assert_eq!(first, other, "reader {worker} walked a different route set");
        }
    }

    /// The table has to be wide enough for the widest disjoint cell, or that
    /// cell is skipped rather than measured on short windows.
    #[test]
    fn coverage_tracks_the_table_width() {
        assert!(TargetSpread::Disjoint.covers(WALK_TARGETS * 16, 16));
        assert!(!TargetSpread::Disjoint.covers(WALK_TARGETS * 15, 16));
        assert!(TargetSpread::Overlapping.covers(WALK_TARGETS, 16), "overlapping needs one window, whatever N is");
    }
}
