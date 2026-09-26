//! `BloomeryHarness`: journal-content scenarios over the shipped bloomery
//! chassis, in process (issue #6264).
//!
//! A scenario states three things. Its **seed** is a list of
//! [`Batch`](aether_bloomery_journal::Batch)es appended in order to a scratch
//! journal before boot; the `Ref`s staging returns are the handles its
//! expected values cite. Its **drive** is the mail it sends the mounted
//! journal owner and bundle driver — [`BloomeryHarness::call`],
//! [`BloomeryHarness::move_head`], [`BloomeryHarness::publish`], and
//! [`BloomeryHarness::settle`], which follows the `AwaitProcessed` →
//! `Processed` protocol to quiescence rather than sleeping. Its **expectation** is the record sequence the loop
//! appended ([`SeededJournal::assert_appended`] over [`Record`]s), plus any
//! view the scenario folds over the actual journal
//! ([`SeededJournal::fold`]). The seed owns those reads because it owns the
//! journal root, so they work the same whether the in-process chassis wrote
//! it or a forked `aether-bloomery` did; [`BloomeryHarness`] forwards each one
//! to the seed it booted over.
//!
//! Expected values are literals the scenario writes, or seed handles. The
//! harness folds only the actual journal and never computes an expected value
//! with the fold code the driver runs, so a bug in a fold cannot move both
//! sides together.
//!
//! The harness boots [`BloomeryChassis`] through `build_mounted`, the
//! composition the `aether-bloomery` binary runs, and addresses the journal
//! owner and the driver through the proven references the mount took back —
//! never by path. Config resolves hermetically, never from the process
//! environment, so HTTP egress stays deny-all unless a scenario opens hosts
//! with [`BloomeryHarness::start_allowing`], or boots with the
//! `aether-bloomery` binary's own flags through
//! [`SeededJournal::boot_with_argv`] — the one way to bind an engine secret
//! (`--secrets-dir`, `--http-secrets`). It never resolves a dist
//! artifact: a scenario that needs fixture wasm reads it through
//! `aether_harness_substrate::test_helpers::require_wasm` and stages the
//! bytes itself.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::mpsc;

use aether_actor::ActorRef;
use aether_bloomery_journal::{Digest, Seq};
use aether_bloomery_view::View;
use aether_chassis_bloomery::{BloomeryChassis, Mounted};
use aether_data::Storage;
use aether_substrate::chassis::builder::BuiltChassis;

mod boot;
mod drive;
mod expect;
mod seed;

pub use drive::{Answer, Pending};
pub use expect::Record;
pub use seed::SeededJournal;

/// A booted bloomery chassis over a seeded scratch journal, with one reply
/// sink every request names as its reply target.
///
/// Built by [`BloomeryHarness::start`] (or [`BloomeryHarness::start_allowing`]
/// when the scenario opens HTTP egress), or by [`SeededJournal::boot`] /
/// [`SeededJournal::boot_with_argv`] when a scenario must observe the journal
/// root before boot or stage the binary's flags. Dropping the harness tears
/// the chassis down before the scratch directory is removed.
pub struct BloomeryHarness {
    chassis: BuiltChassis<BloomeryChassis>,
    mounted: Mounted,
    sink: ActorRef<drive::ReplySink>,
    arrivals: mpsc::Receiver<drive::Arrival>,
    early: HashMap<u64, drive::Reply>,
    correlations: u64,
    /// Declared last, so the chassis drops before the scratch directory the
    /// seed owns is removed.
    journal: SeededJournal,
}

impl BloomeryHarness {
    /// [`SeededJournal::assert_appended`] over the journal the chassis writes.
    ///
    /// # Panics
    ///
    /// As [`SeededJournal::assert_appended`].
    pub fn assert_appended(&self, after: Seq, expected: &[Record]) {
        self.journal.assert_appended(after, expected);
    }

    /// [`SeededJournal::record`] over the journal the chassis writes.
    ///
    /// # Panics
    ///
    /// As [`SeededJournal::record`].
    #[must_use]
    pub fn record<K: Storage>(&self, seq: Seq) -> K {
        self.journal.record(seq)
    }

    /// [`SeededJournal::head`] over the journal the chassis writes.
    ///
    /// # Panics
    ///
    /// As [`SeededJournal::head`].
    #[must_use]
    pub fn head(&self) -> Seq {
        self.journal.head()
    }

    /// [`SeededJournal::stores`] over the journal the chassis writes.
    ///
    /// # Panics
    ///
    /// As [`SeededJournal::stores`].
    #[must_use]
    pub fn stores(&self, digest: &Digest) -> bool {
        self.journal.stores(digest)
    }

    /// [`SeededJournal::fold`] over the journal the chassis writes.
    ///
    /// # Panics
    ///
    /// As [`SeededJournal::fold`].
    #[must_use]
    pub fn fold<V: View>(&self) -> V {
        self.journal.fold()
    }
}
