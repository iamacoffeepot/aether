//! The seed: a scratch journal holding the scenario's batches before boot.

use std::path::{Path, PathBuf};

use aether_bloomery_journal::{Batch, Journal};
use tempfile::TempDir;

/// A scratch journal the harness owns, seeded and not yet booted.
///
/// [`BloomeryHarness::start`](crate::BloomeryHarness::start) seeds and boots
/// in one step; this split exists for a scenario that must observe the
/// journal file between the two.
pub struct SeededJournal {
    pub(crate) scratch: TempDir,
    pub(crate) journal: PathBuf,
}

impl SeededJournal {
    /// Append each batch, in order, at the running head of a fresh journal in
    /// a scratch directory this seed owns.
    ///
    /// An empty seed opens nothing, so the journal file does not exist until
    /// the chassis boots over it: boot then takes the first-run path.
    ///
    /// # Panics
    ///
    /// Panics when the scratch directory cannot be created or a batch does not
    /// append — a scenario whose seed does not hold has nothing to test.
    #[must_use]
    pub fn new(batches: impl IntoIterator<Item = Batch>) -> Self {
        let scratch = tempfile::tempdir().expect("a scratch directory for the seeded journal");
        let journal = scratch.path().join("journal.sqlite");
        let mut batches = batches.into_iter().peekable();
        if batches.peek().is_some() {
            let mut seed = Journal::open(&journal).expect("open the seed journal");
            for (index, batch) in batches.enumerate() {
                let head = seed.head().expect("read the seed journal head");
                seed.append(head, &batch).unwrap_or_else(|error| panic!("append seed batch {index}: {error}"));
            }
        }
        Self { scratch, journal }
    }

    /// The journal file the chassis will open.
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal
    }
}
