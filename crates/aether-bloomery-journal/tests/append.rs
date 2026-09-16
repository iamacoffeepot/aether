//! Append fence, batch range, and all-or-nothing insert.

mod common;

use std::error::Error;

use aether_bloomery_journal::{AppendError, Draft, Journal, Seq};
use common::FixedClock;

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

#[test]
fn an_append_against_a_stale_expected_head_returns_head_moved_and_does_not_write() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    journal.append(Seq(0), &[Note::draft("first")])?;
    let before = journal.read(Seq(0), 16)?;

    let error = journal.append(Seq(0), &[Note::draft("stale")]).expect_err("stale fence must fail");
    match error {
        AppendError::HeadMoved { actual } => assert_eq!(actual, Seq(1)),
        AppendError::Journal(other) => panic!("expected HeadMoved, got Journal({other:?})"),
    }
    assert_eq!(journal.head()?, Seq(1));
    assert_eq!(journal.read(Seq(0), 16)?, before);
    Ok(())
}

#[test]
fn a_three_draft_batch_on_an_empty_journal_returns_the_range_and_stamps_the_clock() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(STAMP_MILLIS)))?;
    let range = journal.append(Seq(0), &[Note::draft("a"), Note::draft("b"), Note::draft("c")])?;
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
    journal.append(Seq(0), &[Note::draft("kept")])?;
    let before_head = journal.head()?;
    let before_count = journal.read(Seq(0), 16)?.len();

    let batch = [Note::draft("would-be-second"), Draft::of(&TooLong { n: 1 }, None)?];
    assert!(journal.append(Seq(1), &batch).is_err());
    assert_eq!(journal.head()?, before_head);
    assert_eq!(journal.read(Seq(0), 16)?.len(), before_count);
    Ok(())
}
