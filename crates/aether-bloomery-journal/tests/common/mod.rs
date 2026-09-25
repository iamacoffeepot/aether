//! Shared fixed clock and temporary journal roots for journal tests.

use std::error::Error;

use aether_bloomery_journal::{Clock, Journal};
use tempfile::TempDir;

/// Clock that always returns the milliseconds it was constructed with.
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

/// A journal over a fresh temporary directory as its root, stamping
/// `now_millis`. The directory is removed when the returned `TempDir` drops,
/// so keep it alive beside the journal.
pub fn temp_journal(now_millis: u64) -> Result<(TempDir, Journal), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open_with_clock(root.path(), Box::new(FixedClock(now_millis)))?;
    Ok((root, journal))
}
