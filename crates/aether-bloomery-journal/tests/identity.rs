//! Process-local [`JournalIdentity`]: move stability and distinct allocations.

mod common;

use std::error::Error;

use aether_bloomery_journal::{AppendError, Batch, Journal, JournalIdentity, Seq};
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

#[test]
fn a_prepared_append_can_commit_after_moving_its_journal() -> Result<(), Box<dyn Error>> {
    let journal = memory()?;
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"moved");
    let prepared = journal.prepare_append(Seq(0), batch)?;
    let (mut moved, _) = take(journal);

    assert_eq!(moved.commit_prepared(prepared)?, Seq(1)..Seq(1));
    assert_eq!(moved.get_bytes(&staged.digest())?.map(|(_, bytes)| bytes), Some(b"moved".to_vec()));
    Ok(())
}

#[test]
fn a_prepared_append_refuses_a_different_in_memory_journal() -> Result<(), Box<dyn Error>> {
    let first = memory()?;
    let mut second = memory()?;
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"foreign");
    let prepared = first.prepare_append(Seq(0), batch)?;

    assert!(matches!(second.commit_prepared(prepared), Err(AppendError::WrongJournal)));
    assert_eq!(first.head()?, Seq(0));
    assert_eq!(second.head()?, Seq(0));
    assert_eq!(second.get_bytes(&staged.digest())?, None);
    Ok(())
}

#[test]
fn a_prepared_append_refuses_another_handle_to_the_same_file() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("journal.sqlite");
    let first = Journal::open_with_clock(&path, Box::new(FixedClock(0)))?;
    let mut second = Journal::open_with_clock(&path, Box::new(FixedClock(0)))?;
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"foreign file handle");
    let prepared = first.prepare_append(Seq(0), batch)?;

    assert!(matches!(second.commit_prepared(prepared), Err(AppendError::WrongJournal)));
    assert_eq!(first.head()?, Seq(0));
    assert_eq!(second.head()?, Seq(0));
    assert_eq!(second.get_bytes(&staged.digest())?, None);
    Ok(())
}
