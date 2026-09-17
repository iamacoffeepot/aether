//! Append fence, batch range, and all-or-nothing insert.

mod common;

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_bloomery_journal::{AppendError, Batch, Clock, Digest, Draft, Journal, JournalError, OpaqueBytes, Ref, Seq};
use aether_data::Kind;
use common::FixedClock;

fn batch_from_drafts(drafts: impl IntoIterator<Item = Draft>) -> Batch {
    let mut batch = Batch::new();
    for draft in drafts {
        batch.push_draft(draft);
    }
    batch
}

const STAMP_MILLIS: u64 = 1_700_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.note")]
struct Note {
    text: String,
}

impl Note {
    fn draft(text: &str) -> Draft {
        Draft::of(&Self { text: text.to_owned() }, None).expect("encode note")
    }
}

/// Kind name of 257 bytes: the `entries.kind` CHECK is `length(kind) <= 256`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(
    name = "test.journal.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
)]
struct TooLong {
    n: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.prepared_reference")]
struct ReferencesBlob {
    blob: Ref<OpaqueBytes>,
}

struct AdvancingClock(Arc<AtomicU64>);

impl Clock for AdvancingClock {
    fn now_millis(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[test]
fn prepared_entries_match_the_committed_envelopes_after_the_clock_advances() -> Result<(), Box<dyn Error>> {
    let clock = Arc::new(AtomicU64::new(STAMP_MILLIS));
    let mut journal = Journal::open_in_memory_with_clock(Box::new(AdvancingClock(Arc::clone(&clock))))?;
    journal.append(Seq(0), &batch_from_drafts([Note::draft("existing")]))?;

    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"candidate artifact");
    batch.push_draft(Draft::of(&Note { text: "first".to_owned() }, Some(Seq(1)))?);
    batch.push_draft(Note::draft("second"));
    let prepared = journal.prepare_append(Seq(1), batch)?;
    assert_eq!(prepared.range(), Seq(2)..Seq(4));
    assert_eq!(prepared.entries().iter().map(|entry| entry.seq).collect::<Vec<_>>(), [Seq(2), Seq(3)]);
    assert_eq!(prepared.entries()[0].kind, "test.journal.note");
    assert_eq!(prepared.entries()[0].cause, Some(Seq(1)));
    assert_eq!(prepared.entries()[1].cause, None);
    assert!(prepared.entries().iter().all(|entry| entry.recorded_at_millis == STAMP_MILLIS));
    assert!(prepared.staged_blob(&staged.digest()).is_some());
    let preview = prepared.entries().to_vec();

    clock.store(STAMP_MILLIS + 500, Ordering::SeqCst);
    assert_eq!(journal.commit_prepared(prepared)?, Seq(2)..Seq(4));
    assert_eq!(journal.read(Seq(1), 10)?, preview);
    assert_eq!(journal.get_bytes(&staged.digest())?, Some((OpaqueBytes::ID, b"candidate artifact".to_vec())));
    Ok(())
}

#[test]
fn discarding_a_preparation_does_not_append_its_artifact_or_event() -> Result<(), Box<dyn Error>> {
    let journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"discarded");
    batch.push_draft(Note::draft("discarded"));
    drop(journal.prepare_append(Seq(0), batch)?);

    assert_eq!(journal.head()?, Seq(0));
    assert!(journal.read(Seq(0), 1)?.is_empty());
    assert_eq!(journal.get_bytes(&staged.digest())?, None);
    Ok(())
}

#[test]
fn a_competing_append_refuses_the_prepared_fence_without_writing() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"stale artifact");
    batch.push_draft(Note::draft("stale"));
    let prepared = journal.prepare_append(Seq(0), batch)?;
    journal.append(Seq(0), &batch_from_drafts([Note::draft("winner")]))?;

    match journal.commit_prepared(prepared).expect_err("stale preparation must fail") {
        AppendError::HeadMoved { actual } => assert_eq!(actual, Seq(1)),
        other => panic!("expected HeadMoved, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(1));
    assert_eq!(Journal::decode::<Note>(&journal.read(Seq(0), 10)?[0])?.text, "winner");
    assert_eq!(journal.get_bytes(&staged.digest())?, None);
    Ok(())
}

#[test]
fn prepared_citation_failure_rolls_back_staged_artifacts_and_events() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"rollback");
    let missing = Ref::<OpaqueBytes>::from_digest(Digest::from_bytes([7; 32]));
    batch.push_event(&ReferencesBlob { blob: missing }, None)?;
    let prepared = journal.prepare_append(Seq(0), batch)?;

    match journal.commit_prepared(prepared).expect_err("dangling citation must fail") {
        AppendError::DanglingRef { digest, expected } => {
            assert_eq!(digest, missing.digest());
            assert_eq!(expected, OpaqueBytes::ID);
        }
        other => panic!("expected DanglingRef, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    assert_eq!(journal.get_bytes(&staged.digest())?, None);
    Ok(())
}

#[test]
fn prepared_sql_constraint_failure_rolls_back_earlier_event_and_artifact() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"rollback");
    batch.push_draft(Note::draft("first"));
    batch.push_draft(Draft::of(&TooLong { n: 1 }, None)?);
    let prepared = journal.prepare_append(Seq(0), batch)?;

    assert!(matches!(journal.commit_prepared(prepared), Err(AppendError::Journal(_))));
    assert_eq!(journal.head()?, Seq(0));
    assert!(journal.read(Seq(0), 10)?.is_empty());
    assert_eq!(journal.get_bytes(&staged.digest())?, None);
    Ok(())
}

#[test]
fn preparation_rejects_values_outside_sqlite_integer_range() -> Result<(), Box<dyn Error>> {
    let journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(u64::MAX)))?;
    let timestamp_result = journal.prepare_append(Seq(0), batch_from_drafts([Note::draft("future")]));
    assert!(matches!(timestamp_result, Err(AppendError::Journal(JournalError::IntegerRange))));

    let journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    let draft = Draft::of(&Note { text: "bad cause".to_owned() }, Some(Seq(u64::MAX)))?;
    let cause_result = journal.prepare_append(Seq(0), batch_from_drafts([draft]));
    assert!(matches!(cause_result, Err(AppendError::Journal(JournalError::IntegerRange))));
    assert_eq!(journal.head()?, Seq(0));
    Ok(())
}

#[test]
fn an_append_against_a_stale_expected_head_returns_head_moved_and_does_not_write() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    journal.append(Seq(0), &batch_from_drafts([Note::draft("first")]))?;
    let before = journal.read(Seq(0), 16)?;

    let error = journal.append(Seq(0), &batch_from_drafts([Note::draft("stale")])).expect_err("stale fence must fail");
    match error {
        AppendError::HeadMoved { actual } => assert_eq!(actual, Seq(1)),
        other => panic!("expected HeadMoved, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(1));
    assert_eq!(journal.read(Seq(0), 16)?, before);
    Ok(())
}

#[test]
fn a_three_draft_batch_on_an_empty_journal_returns_the_range_and_stamps_the_clock() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    let range = journal.append(Seq(0), &batch_from_drafts([Note::draft("a"), Note::draft("b"), Note::draft("c")]))?;
    assert_eq!(range, Seq(1)..Seq(4));
    assert_eq!(journal.head()?, Seq(3));

    let entries = journal.read(Seq(0), 10)?;
    assert_eq!(entries.len(), 3);
    let expected = ["a", "b", "c"];
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry.seq, Seq(u64::try_from(index + 1)?));
        assert_eq!(entry.kind, "test.journal.note");
        assert_eq!(entry.cause, None);
        assert_eq!(entry.recorded_at_millis, STAMP_MILLIS);
        assert_eq!(Journal::decode::<Note>(entry)?.text, expected[index]);
    }
    Ok(())
}

#[test]
fn a_batch_that_fails_midway_on_the_kind_length_check_leaves_head_and_count_unchanged() -> Result<(), Box<dyn Error>> {
    // Drive: entries.kind CHECK (length <= 256). The second draft is TooLong's
    // 257-byte name; the first insert would succeed, then the CHECK fails, and
    // the transaction rolls back.
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    journal.append(Seq(0), &batch_from_drafts([Note::draft("kept")]))?;
    let before_head = journal.head()?;
    let before_count = journal.read(Seq(0), 16)?.len();

    let batch = batch_from_drafts([Note::draft("would-be-second"), Draft::of(&TooLong { n: 1 }, None)?]);
    assert!(journal.append(Seq(1), &batch).is_err());
    assert_eq!(journal.head()?, before_head);
    assert_eq!(journal.read(Seq(0), 16)?.len(), before_count);
    Ok(())
}
