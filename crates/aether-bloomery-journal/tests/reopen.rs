//! Reopening a journal root preserves the head.

mod common;

use std::error::Error;
use std::fs;

use aether_bloomery_journal::{Batch, Draft, Journal, JournalError, Seq};
use aether_bloomery_kinds::{Digest, Head, RecordedHeadMove, Ref, Tree};
use aether_data::{Kind, Storage, StorageData, storage_kind_id_from_name};
use common::FixedClock;
use rusqlite::params;

fn batch_from_drafts(drafts: impl IntoIterator<Item = Draft>) -> Batch {
    let mut batch = Batch::new();
    for draft in drafts {
        batch.push_draft(draft);
    }
    batch
}

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
fn a_journal_closed_and_reopened_at_the_same_root_reports_the_same_head() -> Result<(), Box<dyn Error>> {
    let (root, mut journal) = common::temp_journal(0)?;
    journal.append(Seq(0), &batch_from_drafts([Note::draft("persist")]))?;
    assert_eq!(journal.head()?, Seq(1));
    drop(journal);

    let conn = rusqlite::Connection::open(root.path().join("journal.sqlite"))?;
    let (storage_class, length, bytes): (String, i64, Vec<u8>) =
        conn.query_row("SELECT typeof(kind), length(kind), kind FROM entries WHERE seq = 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    assert_eq!(storage_class, "blob");
    assert_eq!(length, 8);
    assert_eq!(bytes.as_slice(), Note::ID.0.to_le_bytes().as_slice());
    drop(conn);

    let journal = Journal::open_with_clock(root.path(), Box::new(FixedClock(0)))?;
    assert_eq!(journal.head()?, Seq(1));
    let entry = journal.read(Seq(0), 1)?.into_iter().next().expect("persisted entry");
    assert_eq!(entry.kind, Note::ID);
    assert_eq!(Journal::decode::<Note>(&entry)?.text, "persist");
    Ok(())
}

const LEGACY_ENTRIES_DDL: &str = "
CREATE TABLE entries (
    seq INTEGER PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (length(kind) > 0 AND length(kind) <= 256),
    cause INTEGER,
    recorded_at_millis INTEGER NOT NULL,
    bytes BLOB NOT NULL
);";

#[test]
fn legacy_text_rows_read_and_new_blob_rows_append_without_migration() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("journal.sqlite");
    let conn = rusqlite::Connection::open(&path)?;
    conn.execute_batch(LEGACY_ENTRIES_DDL)?;
    conn.execute(
        "INSERT INTO entries (seq, kind, recorded_at_millis, bytes) VALUES (?1, ?2, 0, ?3)",
        params![1, Note::NAME, Note::encode_storage(&StorageData::from_value(Note { text: "old".into() }))?],
    )?;
    conn.execute(
        "INSERT INTO entries (seq, kind, recorded_at_millis, bytes) VALUES (?1, ?2, 0, X'')",
        params![2, "test.journal.unknown"],
    )?;
    conn.execute(
        "INSERT INTO entries (seq, kind, recorded_at_millis, bytes) VALUES (?1, ?2, 0, ?3)",
        params![
            3,
            RecordedHeadMove::NAME,
            RecordedHeadMove::encode_storage(&StorageData::from_value(RecordedHeadMove::from(
                &Head::<Tree>::new("legacy").move_to(Ref::from_digest(Digest::from_bytes([7; 32])))
            )))?
        ],
    )?;
    drop(conn);

    let mut journal = Journal::open_with_clock(root.path(), Box::new(FixedClock(0)))?;
    let before = journal.read(Seq(0), 8)?;
    assert_eq!(before[0].kind, Note::ID);
    assert_eq!(Journal::decode::<Note>(&before[0])?.text, "old");
    assert_eq!(before[1].kind, storage_kind_id_from_name("test.journal.unknown"));
    assert_eq!(before[2].kind, RecordedHeadMove::ID);
    let moved = Journal::decode::<RecordedHeadMove>(&before[2])?;
    assert_eq!(moved.head().as_str(), "legacy");
    assert_eq!(moved.to(), Digest::from_bytes([7; 32]));
    journal.append(Seq(3), &batch_from_drafts([Note::draft("new")]))?;
    drop(journal);

    let conn = rusqlite::Connection::open(&path)?;
    assert_eq!(
        conn.query_row("SELECT typeof(kind) FROM entries WHERE seq = 4", [], |row| row.get::<_, String>(0))?,
        "blob"
    );
    drop(conn);
    let journal = Journal::open_with_clock(root.path(), Box::new(FixedClock(0)))?;
    let entries = journal.read(Seq(0), 8)?;
    assert_eq!(entries.len(), 4);
    assert_eq!(&entries[0..3], before.as_slice());
    assert_eq!(entries[3].kind, Note::ID);
    assert_eq!(Journal::decode::<Note>(&entries[3])?.text, "new");
    Ok(())
}

#[test]
fn corrupt_legacy_kind_values_are_rejected() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("journal.sqlite");
    let conn = rusqlite::Connection::open(&path)?;
    conn.execute_batch(LEGACY_ENTRIES_DDL)?;
    conn.execute_batch(
        "INSERT INTO entries (seq, kind, recorded_at_millis, bytes) VALUES (1, X'01020304050607', 0, X'');
         INSERT INTO entries (seq, kind, recorded_at_millis, bytes) VALUES (2, CAST(X'FF' AS TEXT), 0, X'');",
    )?;
    drop(conn);

    let journal = Journal::open_with_clock(root.path(), Box::new(FixedClock(0)))?;
    for (since, reason) in [(Seq(0), "kind blob is not eight bytes"), (Seq(1), "legacy kind name is not UTF-8")] {
        match journal.read(since, 1).expect_err("corrupt kind must fail") {
            JournalError::CorruptEntryKind(actual) => assert_eq!(actual, reason),
            other => panic!("expected corrupt entry kind, got {other:?}"),
        }
    }

    let other = root.path().join("other-class");
    fs::create_dir(&other)?;
    let conn = rusqlite::Connection::open(other.join("journal.sqlite"))?;
    conn.execute_batch(&LEGACY_ENTRIES_DDL.replace("kind TEXT NOT NULL", "kind NOT NULL"))?;
    conn.execute_batch("INSERT INTO entries (seq, kind, recorded_at_millis, bytes) VALUES (1, 123, 0, X'')")?;
    drop(conn);
    let journal = Journal::open_with_clock(&other, Box::new(FixedClock(0)))?;
    assert!(matches!(
        journal.read(Seq(0), 1),
        Err(JournalError::CorruptEntryKind("kind is neither a blob nor legacy text"))
    ));
    Ok(())
}
