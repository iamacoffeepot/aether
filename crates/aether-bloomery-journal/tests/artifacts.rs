//! Artifact kinds and prefix-aware reads.

mod common;

use std::error::Error;

use aether_bloomery_journal::{Batch, Digest, GetError, OpaqueBytes, Seq, Utf8Text};
use aether_data::Kind;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.note")]
struct Note {
    text: String,
}

#[test]
fn the_same_payload_staged_under_two_kinds_yields_two_blobs() -> Result<(), Box<dyn Error>> {
    // Catches a store that hashes the payload without the prefix, which
    // collapses the two and silently makes one kind win.
    let (_root, mut journal) = common::temp_journal(0)?;
    let payload = "shared";
    let mut batch = Batch::new();
    let as_bytes = batch.stage_bytes(payload.as_bytes());
    let as_text = batch.stage_text(payload);
    assert_ne!(as_bytes.digest(), as_text.digest());
    journal.append(Seq(0), &batch)?;

    assert_eq!(journal.get_bytes(&as_bytes.digest())?, Some((OpaqueBytes::ID, payload.as_bytes().to_vec())));
    assert_eq!(journal.get_bytes(&as_text.digest())?, Some((Utf8Text::ID, payload.as_bytes().to_vec())));
    Ok(())
}

#[test]
fn the_two_leaf_kinds_have_different_ids() {
    // Their ids come from their names. A #[derive(Kind)] that hashes canonical
    // schema bytes would give two unit-like markers the same id, after which
    // every Ref<OpaqueBytes> would accept a text blob and the prefix check
    // would be decoration.
    assert_ne!(OpaqueBytes::ID, Utf8Text::ID);
}

#[test]
fn get_returns_the_value_for_a_matching_prefix_refuses_a_wrong_prefix_and_none_when_absent()
-> Result<(), Box<dyn Error>> {
    // The refusal is what earns the test: a prefix-blind decode would accept
    // any same-shape payload.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut batch = Batch::new();
    let encoded = batch.stage_encoded(&Note { text: "hello".into() })?;
    let text = batch.stage_text("hello");
    journal.append(Seq(0), &batch)?;

    assert_eq!(journal.get::<Note>(&encoded.digest())?, Some(Note { text: "hello".into() }));
    match journal.get::<Note>(&text.digest()).expect_err("wrong prefix must be refused") {
        GetError::PrefixMismatch { expected, actual } => {
            assert_eq!(expected, Note::ID);
            assert_eq!(actual, Utf8Text::ID);
        }
        other => panic!("expected PrefixMismatch, got {other:?}"),
    }
    assert_eq!(journal.get::<Note>(&Digest::from_bytes([0; 32]))?, None);
    Ok(())
}

#[test]
fn get_bytes_many_keeps_absent_slots_in_input_order() -> Result<(), Box<dyn Error>> {
    // The positional mapping is this crate's own logic; a batch that drops the
    // absent slot or reorders is the bug.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut batch = Batch::new();
    let a = batch.stage_bytes(b"present-a");
    let b = batch.stage_bytes(b"present-b");
    journal.append(Seq(0), &batch)?;
    let missing = Digest::from_bytes([1; 32]);

    let many = journal.get_bytes_many(&[a.digest(), missing, b.digest()])?;
    assert_eq!(many.len(), 3);
    assert_eq!(many[0], Some((OpaqueBytes::ID, b"present-a".to_vec())));
    assert_eq!(many[1], None);
    assert_eq!(many[2], Some((OpaqueBytes::ID, b"present-b".to_vec())));
    assert_eq!(many[0], journal.get_bytes(&a.digest())?);
    assert_eq!(many[1], journal.get_bytes(&missing)?);
    assert_eq!(many[2], journal.get_bytes(&b.digest())?);
    Ok(())
}
