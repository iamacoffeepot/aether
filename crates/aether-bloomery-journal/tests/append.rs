//! Append fence, batch range, and all-or-nothing insert.

mod common;

use std::error::Error;

use aether_bloomery_journal::{AppendError, Batch, Draft, Journal, Seq};
use aether_data::Kind;

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

#[test]
fn an_append_against_a_stale_expected_head_returns_head_moved_and_does_not_write() -> Result<(), Box<dyn Error>> {
    let (_root, mut journal) = common::temp_journal(STAMP_MILLIS)?;
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
    let (_root, mut journal) = common::temp_journal(STAMP_MILLIS)?;
    let range = journal.append(Seq(0), &batch_from_drafts([Note::draft("a"), Note::draft("b"), Note::draft("c")]))?;
    assert_eq!(range, Seq(1)..Seq(4));
    assert_eq!(journal.head()?, Seq(3));

    let entries = journal.read(Seq(0), 10)?;
    assert_eq!(entries.len(), 3);
    let expected = ["a", "b", "c"];
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry.seq, Seq(u64::try_from(index + 1)?));
        assert_eq!(entry.kind, Note::ID);
        assert_eq!(entry.cause, None);
        assert_eq!(entry.recorded_at_millis, STAMP_MILLIS);
        assert_eq!(Journal::decode::<Note>(entry)?.text, expected[index]);
    }
    Ok(())
}

#[test]
fn fresh_schema_requires_an_eight_byte_blob_kind() -> Result<(), Box<dyn Error>> {
    let (root, journal) = common::temp_journal(STAMP_MILLIS)?;
    drop(journal);
    let conn = rusqlite::Connection::open(root.path().join("journal.sqlite"))?;
    let insert = "INSERT INTO entries (seq, kind, recorded_at_millis, bytes) VALUES (1, ?1, 0, X'')";
    assert!(conn.execute(insert, [b"short".as_slice()]).is_err());
    assert!(conn.execute(insert, [b"ninebytes".as_slice()]).is_err());
    assert!(conn.execute(insert, ["legacy.text"]).is_err());
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get::<_, i64>(0))?, 0);
    Ok(())
}
