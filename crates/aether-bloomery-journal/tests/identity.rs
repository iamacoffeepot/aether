//! Process-local [`JournalIdentity`]: move stability and distinct allocations.

mod common;

use std::error::Error;

use aether_bloomery_journal::{Journal, JournalIdentity};
use common::FixedClock;

fn memory() -> Result<Journal, Box<dyn Error>> {
    Ok(Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?)
}

fn take(journal: Journal) -> (Journal, JournalIdentity) {
    let identity = journal.identity();
    (journal, identity)
}

#[test]
fn moving_a_journal_preserves_identity() -> Result<(), Box<dyn Error>> {
    // Bug: identity was a unit value or a pointer to a moved field, so a moved journal looks new.
    let journal = memory()?;
    let before = journal.identity();
    let (journal, after_move) = take(journal);
    assert_eq!(before, after_move);
    assert_eq!(before, journal.identity());
    Ok(())
}

#[test]
fn distinct_in_memory_journals_have_distinct_identities() -> Result<(), Box<dyn Error>> {
    // Bug: identity is a unit token, so two journals compare equal and a registry would reuse caches.
    let first = memory()?;
    let second = memory()?;
    assert_ne!(first.identity(), second.identity());
    assert_eq!(first.identity(), first.identity());
    Ok(())
}

#[test]
fn reopening_the_same_file_mints_a_new_identity() -> Result<(), Box<dyn Error>> {
    // Bug: identity is a path or file id, so a reopen reuses another process's view caches.
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("journal.sqlite");
    let first = Journal::open_with_clock(&path, Box::new(FixedClock(0)))?;
    let first_id = first.identity();
    drop(first);
    let second = Journal::open_with_clock(&path, Box::new(FixedClock(0)))?;
    assert_ne!(first_id, second.identity());
    Ok(())
}
