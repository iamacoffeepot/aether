//! Process-local [`JournalIdentity`]: move stability and distinct allocations.

mod common;

use std::error::Error;

use aether_bloomery_journal::{Journal, JournalIdentity};
use common::FixedClock;

fn take(journal: Journal) -> (Journal, JournalIdentity) {
    let identity = journal.identity();
    (journal, identity)
}

#[test]
fn moving_a_journal_preserves_identity() -> Result<(), Box<dyn Error>> {
    // Bug: identity was a unit value or a pointer to a moved field, so a moved journal looks new.
    let (_root, journal) = common::temp_journal(0)?;
    let before = journal.identity();
    let (journal, after_move) = take(journal);
    assert_eq!(before, after_move);
    assert_eq!(before, journal.identity());
    Ok(())
}

#[test]
fn journals_over_distinct_roots_have_distinct_identities() -> Result<(), Box<dyn Error>> {
    // Bug: identity is a unit token, so two journals compare equal and a registry would reuse caches.
    let (_first_root, first) = common::temp_journal(0)?;
    let (_second_root, second) = common::temp_journal(0)?;
    assert_ne!(first.identity(), second.identity());
    assert_eq!(first.identity(), first.identity());
    Ok(())
}

#[test]
fn reopening_the_same_root_mints_a_new_identity() -> Result<(), Box<dyn Error>> {
    // Bug: identity is a path or file id, so a reopen reuses another process's view caches.
    let (root, first) = common::temp_journal(0)?;
    let first_id = first.identity();
    drop(first);
    let second = Journal::open_with_clock(root.path(), Box::new(FixedClock(0)))?;
    assert_ne!(first_id, second.identity());
    Ok(())
}
