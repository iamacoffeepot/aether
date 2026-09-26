//! The journal root: layout, exclusive lock, temp sweep, blob files, and the lock-free reader (ADR-0220).

mod common;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery_journal::{
    AppendError, Batch, Digest, Journal, JournalError, JournalReader, OpaqueBytes, Ref, Seq, artifact_blob,
    artifact_digest,
};
use aether_bloomery_kinds::Tree;
use aether_data::Kind;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.root.pointer")]
struct Pointer {
    target: Ref<OpaqueBytes>,
}

fn blob_path(root: &Path, digest: &Digest) -> PathBuf {
    let hex = digest.to_string();
    root.join("blobs").join(&hex[..2]).join(hex)
}

fn staged_bytes(payload: &[u8]) -> Batch {
    let mut batch = Batch::new();
    batch.stage_bytes(payload);
    batch
}

#[test]
fn a_fresh_open_creates_the_root_and_its_layout() -> Result<(), Box<dyn Error>> {
    // Catches a missing `blobs/tmp/`, which fails the first blob write, and a
    // root that is not created when its parent exists.
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("journal");
    let _journal = Journal::open(&root)?;

    assert!(root.join("journal.sqlite").is_file());
    assert!(root.join("blobs").is_dir());
    assert!(root.join("blobs").join("tmp").is_dir());
    assert!(root.join("lock").is_file());
    Ok(())
}

#[test]
fn a_fresh_root_holds_the_empty_tree_and_no_entry() -> Result<(), Box<dyn Error>> {
    // Catches the seed recorded as an entry (the head moves), the seed
    // written only when the root is created, and a reopen that trips the
    // digest primary key because the absent-row check was skipped.
    let (root, journal) = common::temp_journal(0)?;
    let empty = Ref::of_encoded(&Tree::empty())?.digest();
    assert_eq!(journal.get::<Tree>(&empty)?, Some(Tree::empty()));
    assert_eq!(journal.head()?, Seq(0));

    drop(journal);
    let reopened = Journal::open(root.path())?;
    assert_eq!(reopened.get::<Tree>(&empty)?, Some(Tree::empty()));
    assert_eq!(reopened.head()?, Seq(0));
    Ok(())
}

#[test]
fn a_held_root_refuses_a_second_open_until_the_first_journal_drops() -> Result<(), Box<dyn Error>> {
    // Catches a per-process lock (POSIX record locks let the same process
    // open twice) and a lock that is not released when the journal drops.
    let (root, first) = common::temp_journal(0)?;

    match Journal::open(root.path()) {
        Err(JournalError::Locked { root: named }) => assert_eq!(named, root.path()),
        Err(other) => panic!("expected Locked, got {other:?}"),
        Ok(_) => panic!("a second open of a held root must fail"),
    }

    drop(first);
    Journal::open(root.path())?;
    Ok(())
}

#[test]
fn a_regular_file_at_the_root_path_is_not_a_directory() -> Result<(), Box<dyn Error>> {
    // Catches a single-file journal opened as a root, or overwritten by one.
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("journal.sqlite");
    fs::write(&root, b"an old single-file journal")?;

    match Journal::open(&root) {
        Err(JournalError::NotADirectory { root: named }) => assert_eq!(named, root),
        Err(other) => panic!("expected NotADirectory, got {other:?}"),
        Ok(_) => panic!("a regular file must not open as a journal root"),
    }
    assert_eq!(fs::read(&root)?, b"an old single-file journal");
    Ok(())
}

#[test]
fn a_root_whose_parent_is_missing_refuses_open() -> Result<(), Box<dyn Error>> {
    // Catches the root created with its missing parents: a typo in the
    // configured path would then silently start an empty journal elsewhere.
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("missing-parent").join("journal");

    match Journal::open(&root) {
        Err(JournalError::Io { path, .. }) => assert_eq!(path, root),
        Err(other) => panic!("expected an i/o refusal naming the root, got {other:?}"),
        Ok(_) => panic!("a root whose parent is missing must not open"),
    }
    assert!(!temp.path().join("missing-parent").exists());
    Ok(())
}

#[test]
fn open_sweeps_what_an_interrupted_write_left_in_tmp() -> Result<(), Box<dyn Error>> {
    // Catches a sweep that is skipped, or that leaves nested entries behind.
    let (root, journal) = common::temp_journal(0)?;
    drop(journal);
    let tmp = root.path().join("blobs").join("tmp");
    fs::write(tmp.join("half-written"), b"partial")?;
    fs::create_dir(tmp.join("stray"))?;
    fs::write(tmp.join("stray").join("inner"), b"partial")?;

    let _journal = Journal::open(root.path())?;
    assert_eq!(fs::read_dir(&tmp)?.count(), 0);
    Ok(())
}

#[test]
fn an_appended_blob_is_a_digest_named_file_and_its_row_holds_no_bytes() -> Result<(), Box<dyn Error>> {
    // Catches bytes still stored inline in the row, a file written under the
    // wrong name or shard, and a file that is not the prefixed blob that was hashed.
    let (root, mut journal) = common::temp_journal(0)?;
    journal.append(Seq(0), &staged_bytes(b"blob payload"))?;

    let digest = artifact_digest(OpaqueBytes::ID, b"blob payload");
    assert_eq!(fs::read(blob_path(root.path(), &digest))?, artifact_blob(OpaqueBytes::ID, b"blob payload"));

    let conn = rusqlite::Connection::open(root.path().join("journal.sqlite"))?;
    let mut columns = conn.prepare("SELECT name FROM pragma_table_info('artifacts')")?;
    let columns: Vec<String> = columns.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
    assert_eq!(columns, ["digest", "size_bytes", "recorded_at_millis"]);
    Ok(())
}

#[test]
fn a_refused_append_leaves_no_row_for_its_staged_blob() -> Result<(), Box<dyn Error>> {
    // Catches a blob row committed ahead of the refusal, which would make a
    // blob stored whose citation never resolved.
    let (_root, mut journal) = common::temp_journal(0)?;
    let absent = Digest::from_bytes([3; 32]);
    let mut batch = Batch::new();
    let pointer = batch.stage_encoded(&Pointer { target: Ref::from_digest(absent) })?;

    let error = journal.append(Seq(0), &batch).expect_err("a dangling citation must be refused");
    assert!(matches!(error, AppendError::DanglingRef { digest, .. } if digest == absent), "got {error:?}");
    assert_eq!(journal.get_bytes(&pointer.digest())?, None);
    Ok(())
}

#[cfg(unix)]
#[test]
fn an_existing_digest_file_is_never_rewritten() -> Result<(), Box<dyn Error>> {
    // Catches a write that renames over a complete digest-named file instead
    // of skipping to the row.
    use std::os::unix::fs::MetadataExt;

    let (root, mut journal) = common::temp_journal(0)?;
    let digest = artifact_digest(OpaqueBytes::ID, b"already here");
    let path = blob_path(root.path(), &digest);
    fs::create_dir_all(path.parent().ok_or("a shard directory")?)?;
    fs::write(&path, artifact_blob(OpaqueBytes::ID, b"already here"))?;
    let before = fs::metadata(&path)?.ino();

    journal.append(Seq(0), &staged_bytes(b"already here"))?;
    assert_eq!(fs::metadata(&path)?.ino(), before);
    assert_eq!(journal.get_bytes(&digest)?, Some((OpaqueBytes::ID, b"already here".to_vec())));
    Ok(())
}

#[test]
fn a_row_whose_blob_file_is_gone_is_a_missing_blob() -> Result<(), Box<dyn Error>> {
    // Catches a missing file read as an absent artifact, which would hide a
    // stored artifact the log still cites.
    let (root, mut journal) = common::temp_journal(0)?;
    journal.append(Seq(0), &staged_bytes(b"soon deleted"))?;
    let digest = artifact_digest(OpaqueBytes::ID, b"soon deleted");
    fs::remove_file(blob_path(root.path(), &digest))?;

    match journal.get_bytes(&digest) {
        Err(JournalError::MissingBlob(missing)) => assert_eq!(missing, digest),
        other => panic!("expected MissingBlob, got {other:?}"),
    }
    Ok(())
}

#[test]
fn a_reader_sees_each_commit_while_the_writer_holds_the_root_and_after_it_drops() -> Result<(), Box<dyn Error>> {
    // Catches a reader that takes the lock (refused by a live writer), one
    // pinned to the snapshot it opened on, and one that cannot open a root
    // whose writer closed cleanly.
    let (root, mut journal) = common::temp_journal(0)?;
    let reader = JournalReader::open(root.path())?;
    assert_eq!(reader.head()?, Seq(0));

    let mut first = staged_bytes(b"first");
    first.push_event(&Pointer { target: Ref::from_digest(artifact_digest(OpaqueBytes::ID, b"first")) }, None)?;
    journal.append(Seq(0), &first)?;
    assert_eq!(reader.head()?, Seq(1));

    let mut second = Batch::new();
    second.push_event(&Pointer { target: Ref::from_digest(artifact_digest(OpaqueBytes::ID, b"first")) }, None)?;
    journal.append(Seq(1), &second)?;
    assert_eq!(reader.head()?, Seq(2));
    assert_eq!(reader.read(Seq(0), 8)?, journal.read(Seq(0), 8)?);

    drop(journal);
    let digest = artifact_digest(OpaqueBytes::ID, b"first");
    assert_eq!(reader.get_bytes(&digest)?, Some((OpaqueBytes::ID, b"first".to_vec())));
    let fresh = JournalReader::open(root.path())?;
    assert_eq!(fresh.head()?, Seq(2));
    assert_eq!(fresh.get_bytes(&digest)?, Some((OpaqueBytes::ID, b"first".to_vec())));
    Ok(())
}
