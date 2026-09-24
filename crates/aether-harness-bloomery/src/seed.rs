//! The seed: a journal holding the scenario's batches before boot, in a
//! scratch directory or at a path the caller owns.

use std::path::{Path, PathBuf};

use aether_bloomery_journal::{Batch, Journal};
use tempfile::TempDir;

/// A scratch journal the harness owns: seeded before boot, and read back
/// after it.
///
/// [`BloomeryHarness::start`](crate::BloomeryHarness::start) seeds and boots
/// in one step; this split exists for a scenario that must observe the
/// journal file between the two, or that hands the file to an engine outside
/// this process, such as a forked `aether-bloomery`. Either way the
/// expectation reads ([`assert_appended`](Self::assert_appended),
/// [`record`](Self::record), [`head`](Self::head), [`stores`](Self::stores),
/// [`fold`](Self::fold)) open the file afresh, so they see what the engine
/// committed.
///
/// [`SeededJournal::at`] places the journal at a caller-chosen path instead of
/// a scratch directory, for a run whose records must outlive the harness.
pub struct SeededJournal {
    pub(crate) journal: PathBuf,
    /// The scratch directory holding `journal`, removed on drop; `None` for a
    /// journal the caller placed with [`SeededJournal::at`], which is kept.
    _scratch: Option<TempDir>,
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
        seed(&journal, batches);
        Self { journal, _scratch: Some(scratch) }
    }

    /// Append each batch, in order, at the running head of a fresh journal at
    /// `journal`, a path the caller owns.
    ///
    /// Unlike [`SeededJournal::new`], nothing is removed when the seed or the
    /// harness booted over it drops: the file, and every record the engine
    /// appends to it, is kept for the caller to inspect. The parent directory
    /// must already exist. An empty seed opens nothing, as with `new`.
    ///
    /// # Panics
    ///
    /// Panics when `journal` is not absolute or already exists — an existing
    /// file would mix the caller's records into the seed — or when a batch
    /// does not append.
    #[must_use]
    pub fn at(journal: &Path, batches: impl IntoIterator<Item = Batch>) -> Self {
        assert!(journal.is_absolute(), "the seed journal path must be absolute: {}", journal.display());
        assert!(!journal.exists(), "the seed journal must not already exist: {}", journal.display());
        seed(journal, batches);
        Self { journal: journal.to_path_buf(), _scratch: None }
    }

    /// The journal file the engine opens.
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal
    }
}

/// Append each batch, in order, at the running head of the journal at
/// `journal`, opening it only when there is a batch to append.
fn seed(journal: &Path, batches: impl IntoIterator<Item = Batch>) {
    let mut batches = batches.into_iter().peekable();
    if batches.peek().is_some() {
        let mut seed = Journal::open(journal).expect("open the seed journal");
        for (index, batch) in batches.enumerate() {
            let head = seed.head().expect("read the seed journal head");
            seed.append(head, &batch).unwrap_or_else(|error| panic!("append seed batch {index}: {error}"));
        }
    }
}
