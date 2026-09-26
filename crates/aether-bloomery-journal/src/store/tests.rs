//! The streaming artifact store over a real journal root.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use aether_bloomery_kinds::{Name, Node, Tree};
use aether_data::Kind;
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;

use super::ArtifactBatch;
use crate::journal::DATABASE_FILE;
use crate::{AppendError, Batch, Digest, Journal, JournalError, OpaqueBytes, Ref, Seq, artifact_blob, artifact_digest};

type TestResult = Result<(), Box<dyn Error>>;

/// The artifact rows every open root holds: the empty tree the journal seeds.
const SEEDED_ROWS: i64 = 1;

/// A fresh journal over `<temp>/journal`.
fn open_root() -> Result<(TempDir, PathBuf, Journal), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("journal");
    let journal = Journal::open(&root)?;
    Ok((temp, root, journal))
}

fn blob_path(root: &Path, digest: &Digest) -> PathBuf {
    let hex = digest.to_string();
    root.join("blobs").join(&hex[..2]).join(hex)
}

fn tmp_entries(root: &Path) -> Result<usize, Box<dyn Error>> {
    Ok(fs::read_dir(root.join("blobs").join("tmp"))?.count())
}

/// The committed artifact rows, read on a connection of the test's own.
fn artifact_rows(root: &Path) -> Result<i64, Box<dyn Error>> {
    let conn = Connection::open_with_flags(root.join(DATABASE_FILE), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn.query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))?)
}

/// Stream `chunks` as one blob opened with their total length.
fn stream(batch: &mut ArtifactBatch, chunks: &[&[u8]]) -> Result<Ref<OpaqueBytes>, JournalError> {
    let len = chunks.iter().map(|chunk| chunk.len() as u64).sum();
    let mut blob = batch.blob(len)?;
    for chunk in chunks {
        blob.write_chunk(chunk)?;
    }
    blob.finish()
}

fn tree_citing(file: Ref<OpaqueBytes>) -> Result<Tree, Box<dyn Error>> {
    let mut entries = BTreeMap::new();
    entries.insert(Name::new("payload")?, Node::File(file));
    Ok(Tree::new(entries))
}

#[test]
fn a_blob_streamed_in_chunks_is_the_file_and_digest_a_staged_blob_would_be() -> TestResult {
    // Catches a digest drift between the streamed and the whole-slice path: a
    // prefix hashed but not written (or written but not hashed), or a chunk
    // hashed twice, would name the file differently than `artifact_digest`.
    let (_temp, root, journal) = open_root()?;
    let mut batch = journal.artifact_store().batch()?;

    let blob = stream(&mut batch, &[b"first ", b"second ", b"third"])?;

    let payload = b"first second third";
    assert_eq!(blob.digest(), artifact_digest(OpaqueBytes::ID, payload));
    assert_eq!(fs::read(blob_path(&root, &blob.digest()))?, artifact_blob(OpaqueBytes::ID, payload));
    Ok(())
}

#[test]
fn finish_after_a_short_write_is_refused() -> TestResult {
    // Catches a truncated blob named and stored as whole: an export cut off
    // mid-stream must not become an artifact.
    let (_temp, root, journal) = open_root()?;
    let mut batch = journal.artifact_store().batch()?;

    let mut blob = batch.blob(10)?;
    blob.write_chunk(b"four")?;
    match blob.finish() {
        Err(JournalError::BlobLength { expected_bytes: 10, actual_bytes: 4 }) => {}
        other => panic!("expected BlobLength 10 / 4, got {other:?}"),
    }

    batch.commit()?;
    assert_eq!(artifact_rows(&root)?, SEEDED_ROWS);
    Ok(())
}

#[test]
fn dropping_an_unfinished_blob_file_deletes_its_temp_file() -> TestResult {
    // Catches a temp file leak: a worker that abandons a blob mid-stream would
    // otherwise leave its partial bytes in `blobs/tmp/` until the next open.
    let (_temp, root, journal) = open_root()?;
    let mut batch = journal.artifact_store().batch()?;

    let mut blob = batch.blob(10)?;
    blob.write_chunk(b"half ")?;
    assert_eq!(tmp_entries(&root)?, 1, "the blob streams through a temp file");
    drop(blob);

    assert_eq!(tmp_entries(&root)?, 0);
    Ok(())
}

#[test]
fn dropping_an_uncommitted_batch_leaves_no_row() -> TestResult {
    // Catches a partial import made visible: a row inserted as each blob
    // finishes, rather than all of them at commit, would store what the
    // worker gave up on.
    let (_temp, root, journal) = open_root()?;
    let mut batch = journal.artifact_store().batch()?;
    let blob = stream(&mut batch, &[b"abandoned"])?;
    let tree = batch.stage_encoded(&tree_citing(blob)?)?;

    drop(batch);

    assert_eq!(artifact_rows(&root)?, SEEDED_ROWS);
    assert!(journal.get_bytes(&blob.digest())?.is_none());
    assert!(journal.get_bytes(&tree.digest())?.is_none());
    Ok(())
}

#[test]
fn a_staged_tree_citing_an_absent_blob_is_refused_at_commit() -> TestResult {
    // Catches a dangling citation: a commit that inserts rows without the
    // citation check `append` runs would store a tree naming nothing.
    let (_temp, root, journal) = open_root()?;
    let mut batch = journal.artifact_store().batch()?;
    let missing = Ref::<OpaqueBytes>::from_digest(Digest::from_bytes([9; 32]));
    batch.stage_encoded(&tree_citing(missing)?)?;

    match batch.commit() {
        Err(AppendError::DanglingRef { digest, expected }) => {
            assert_eq!(digest, missing.digest());
            assert_eq!(expected, OpaqueBytes::ID);
        }
        other => panic!("expected DanglingRef, got {other:?}"),
    }
    assert_eq!(artifact_rows(&root)?, SEEDED_ROWS);
    Ok(())
}

#[test]
fn a_second_batch_of_the_same_bytes_adds_no_row() -> TestResult {
    // Catches a lost dedup: without the absent-row check a second import of
    // the same bytes fails its commit on the digest key, or duplicates it.
    let (_temp, root, journal) = open_root()?;
    let store = journal.artifact_store();

    let mut first = store.batch()?;
    let blob = stream(&mut first, &[b"same bytes"])?;
    first.commit()?;
    let mut second = store.batch()?;
    assert_eq!(stream(&mut second, &[b"same ", b"bytes"])?, blob);
    second.commit()?;

    assert_eq!(artifact_rows(&root)?, SEEDED_ROWS + 1);
    Ok(())
}

#[test]
fn a_corrupted_blob_file_fails_the_verifying_reader_at_end_of_stream() -> TestResult {
    // Catches silent corruption: a reader that trusts the file would hand a
    // flipped byte to its caller as the committed blob.
    let (_temp, root, journal) = open_root()?;
    let store = journal.artifact_store();
    let mut batch = store.batch()?;
    let blob = stream(&mut batch, &[b"exact ", b"payload"])?;
    batch.commit()?;

    let reader = store.batch()?;
    let mut verified = reader.blob_reader(&blob)?.expect("the committed blob has a row");
    assert_eq!(verified.payload_len(), 13);
    let mut whole = Vec::new();
    verified.read_to_end(&mut whole)?;
    assert_eq!(whole, b"exact payload");

    let path = blob_path(&root, &blob.digest());
    let mut bytes = fs::read(&path)?;
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, bytes)?;

    let mut corrupted = reader.blob_reader(&blob)?.expect("the row is unchanged");
    let error = corrupted.read_to_end(&mut Vec::new()).expect_err("a flipped byte must fail the read");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    Ok(())
}

#[test]
fn append_and_a_batch_commit_on_another_thread_wait_out_each_others_writes() -> TestResult {
    // Catches `SQLITE_BUSY` between the two doors: a connection without a busy
    // timeout fails at once while the other door's write transaction runs,
    // instead of waiting it out. A write transaction held on a third
    // connection stands in for the running commit, so both doors are
    // deterministically inside the wait when it ends.
    let (_temp, root, mut journal) = open_root()?;
    let store = journal.artifact_store();
    let holder = Connection::open(root.join(DATABASE_FILE))?;
    holder.execute_batch("BEGIN IMMEDIATE")?;

    let (started_sender, started) = mpsc::channel();
    let (committed, appended) = thread::scope(|scope| -> Result<_, Box<dyn Error>> {
        let committing = scope.spawn(move || {
            let mut batch = store.batch().expect("open a batch on the worker");
            stream(&mut batch, &[b"from the worker"]).expect("stream the worker's blob");
            started_sender.send(()).expect("signal the test");
            batch.commit()
        });
        started.recv()?;
        let appending = scope.spawn(|| {
            let mut batch = Batch::new();
            batch.stage_text("from the journal actor");
            journal.append(Seq(0), &batch).map(|_| ())
        });
        thread::sleep(Duration::from_millis(200));
        holder.execute_batch("COMMIT")?;
        Ok((committing.join().expect("the committing thread"), appending.join().expect("the appending thread")))
    })?;

    committed?;
    appended?;
    assert_eq!(artifact_rows(&root)?, SEEDED_ROWS + 2);
    Ok(())
}

#[test]
fn a_live_store_keeps_the_root_locked_after_the_journal_drops() -> TestResult {
    // Catches the lock released early: a worker still streaming into the root
    // must not share it with a second journal, whose open would also sweep
    // the worker's in-flight temp files.
    let (_temp, root, journal) = open_root()?;
    let store = journal.artifact_store();
    drop(journal);

    match Journal::open(&root) {
        Err(JournalError::Locked { root: named }) => assert_eq!(named, root),
        Err(other) => panic!("expected Locked, got {other:?}"),
        Ok(_) => panic!("a root a live store holds must not open"),
    }
    drop(store);
    Journal::open(&root)?;
    Ok(())
}
