//! The journal sink and source over a real journal root.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;

use aether_bloomery_journal::Journal;
use aether_bloomery_kinds::{Name, Node, Ref, Tree};
use aether_bloomery_tar::{DecodeError, Limits, Refusal, Rules, decode, encode};

use super::{JournalSink, JournalSource};
use crate::runtime::testing::{TarWriter, artifact_rows};

type TestResult = Result<(), Box<dyn Error>>;

/// Over 1 MiB, so the blob spans many decode copy buffers.
fn large_payload() -> Vec<u8> {
    (0..=250u8).cycle().take(1_536 * 1024).collect()
}

fn rules() -> Result<Rules, Box<dyn Error>> {
    Ok(Rules::userland(Limits::new(1_000, 1 << 30)?))
}

fn directory(entries: Vec<(&str, Node)>) -> Result<Tree, Box<dyn Error>> {
    let mut map = BTreeMap::new();
    for (name, node) in entries {
        map.insert(Name::new(name)?, node);
    }
    Ok(Tree::new(map))
}

#[test]
fn an_export_decodes_into_rows_naming_the_tree_its_content_hashes_to() -> TestResult {
    // Catches a sink whose references drift from the content they store: a
    // blob hashed with the wrong kind prefix, a chunk dropped or repeated
    // across the copy buffer's boundary, or a tree staged under a digest other
    // than its encoding's. The expected tree is built from the content here,
    // never by the sink.
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("journal");
    let store = Journal::open(&root)?.artifact_store();
    let large = large_payload();
    let archive = TarWriter::new().directory("etc/").file("etc/hostname", b"ws\n").file("large.bin", &large).finish();

    let mut batch = store.batch()?;
    let tree = decode(archive.as_slice(), &mut JournalSink::new(&mut batch), &rules()?)?;
    batch.commit()?;

    let etc = directory(vec![("hostname", Node::File(Ref::of_bytes(b"ws\n")))])?;
    let expected = directory(vec![
        ("etc", Node::Directory(Ref::of_encoded(&etc)?)),
        ("large.bin", Node::File(Ref::of_bytes(&large))),
    ])?;
    assert_eq!(tree, Ref::of_encoded(&expected)?);
    assert_eq!(store.batch()?.get::<Tree>(&tree.digest())?, Some(expected), "the root's row decodes to the tree");
    assert_eq!(artifact_rows(&root)?, 4, "two blobs and two trees");
    Ok(())
}

#[test]
fn a_stream_cut_inside_the_large_blob_leaves_no_row_and_no_temp_file() -> TestResult {
    // Catches a partial import made durable: a sink that finishes the blob it
    // was streaming when the input ran dry, or keeps a temp file behind, would
    // leave bytes of a tree nobody can name.
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("journal");
    let store = Journal::open(&root)?.artifact_store();
    let archive = TarWriter::new().file("small", b"small").file("large.bin", &large_payload()).cut(700 * 1024);

    let mut batch = store.batch()?;
    let error = decode(archive.as_slice(), &mut JournalSink::new(&mut batch), &rules()?).expect_err("a cut stream");
    drop(batch);

    assert!(matches!(error, DecodeError::Refused { refusal: Refusal::Truncated, .. }), "{error}");
    assert_eq!(artifact_rows(&root)?, 0);
    assert_eq!(fs::read_dir(root.join("blobs").join("tmp"))?.count(), 0);
    Ok(())
}

#[test]
fn a_decoded_tree_encodes_back_through_the_source_to_the_same_tree() -> TestResult {
    // Catches a source that hands the encoder other bytes than the row
    // stores: a blob cut at a copy-buffer boundary, a length taken from the
    // wrong row, or a tree loaded under the wrong digest. Decoding the encoded
    // stream into a second, empty journal must land on the same digest, which
    // hashes every byte of every blob and tree.
    let temp = tempfile::tempdir()?;
    let store = Journal::open(&temp.path().join("first"))?.artifact_store();
    let archive = TarWriter::new()
        .directory("bin/")
        .file("bin/tool", b"#!tool\n")
        .file("large.bin", &large_payload())
        .symlink("link", "bin/tool")
        .finish();
    let mut batch = store.batch()?;
    let tree = decode(archive.as_slice(), &mut JournalSink::new(&mut batch), &rules()?)?;
    batch.commit()?;

    let reader = store.batch()?;
    let mut encoded = Vec::new();
    encode(&tree, &mut JournalSource::new(&reader), &mut encoded)?;

    let second = Journal::open(&temp.path().join("second"))?.artifact_store();
    let mut batch = second.batch()?;
    let round_trip = decode(encoded.as_slice(), &mut JournalSink::new(&mut batch), &rules()?)?;
    assert_eq!(round_trip, tree);
    Ok(())
}
