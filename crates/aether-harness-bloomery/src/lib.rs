//! `BloomeryHarness`: journal-content scenarios over the shipped bloomery
//! chassis, in process (issue #6264).
//!
//! A scenario states three things. Its **seed** is a list of
//! [`Batch`](aether_bloomery_journal::Batch)es appended in order to a scratch
//! journal before boot; the `Ref`s staging returns are the handles its
//! expected values cite. Its **drive** is the mail it sends the mounted
//! journal owner and bundle driver — [`BloomeryHarness::call`],
//! [`BloomeryHarness::move_head`], and [`BloomeryHarness::settle`], which
//! follows the `AwaitProcessed` → `Processed` protocol to quiescence rather
//! than sleeping. Its **expectation** is the record sequence the loop
//! appended ([`BloomeryHarness::assert_appended`] over [`Record`]s), plus any
//! view the scenario folds over the actual journal
//! ([`BloomeryHarness::fold`]).
//!
//! Expected values are literals the scenario writes, or seed handles. The
//! harness folds only the actual journal and never computes an expected value
//! with the fold code the driver runs, so a bug in a fold cannot move both
//! sides together.
//!
//! The harness boots [`BloomeryChassis`] through `build_mounted`, the
//! composition the `aether-bloomery` binary runs, and addresses the journal
//! owner and the driver through the proven references the mount took back —
//! never by path. It never resolves a dist
//! artifact: a scenario that needs fixture wasm reads it through
//! `aether_harness_substrate::test_helpers::require_wasm` and stages the
//! bytes itself.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

use aether_actor::ActorRef;
use aether_chassis_bloomery::{BloomeryChassis, Mounted};
use aether_substrate::chassis::builder::BuiltChassis;
use tempfile::TempDir;

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
/// Built by [`BloomeryHarness::start`], or by [`SeededJournal::boot`] when a
/// scenario must observe the journal file before boot. Dropping the harness
/// tears the chassis down before the scratch directory is removed.
pub struct BloomeryHarness {
    chassis: BuiltChassis<BloomeryChassis>,
    mounted: Mounted,
    sink: ActorRef<drive::ReplySink>,
    arrivals: mpsc::Receiver<drive::Arrival>,
    early: HashMap<u64, drive::Reply>,
    correlations: u64,
    journal: PathBuf,
    _scratch: TempDir,
}
