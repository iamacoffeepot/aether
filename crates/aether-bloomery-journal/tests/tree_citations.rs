//! Append verifies a tree's children through the hand-written Node Cites walk.

mod common;

use std::collections::BTreeMap;
use std::error::Error;

use aether_bloomery_journal::{AppendError, Batch, Digest, Seq, Utf8Text};
use aether_bloomery_kinds::{Name, Node, OpaqueBytes, Ref, Tree};
use aether_data::Kind;

fn name(value: &str) -> Name {
    Name::new(value).expect("valid name")
}

#[test]
fn a_tree_whose_directory_cites_an_unstaged_tree_is_dangling() -> Result<(), Box<dyn Error>> {
    // Catches Cites for Node forwarding the Directory variant as nothing.
    let (_root, mut journal) = common::temp_journal(0)?;
    let missing = Ref::<Tree>::from_digest(Digest::from_bytes([7; 32]));
    let mut entries = BTreeMap::new();
    entries.insert(name("sub"), Node::Directory(missing));
    let mut batch = Batch::new();
    batch.stage_encoded(&Tree::new(entries))?;

    let error = journal.append(Seq(0), &batch).expect_err("dangling directory must fail");
    match error {
        AppendError::DanglingRef { digest, expected } => {
            assert_eq!(digest, missing.digest());
            assert_eq!(expected, Tree::ID);
        }
        other => panic!("expected DanglingRef, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    Ok(())
}

#[test]
fn a_tree_whose_file_cites_a_text_blob_is_a_prefix_mismatch() -> Result<(), Box<dyn Error>> {
    // Catches Cites for Node forwarding File as the wrong kind, or nothing.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut batch = Batch::new();
    let text = batch.stage_text("hello");
    let as_bytes = Ref::<OpaqueBytes>::from_digest(text.digest());
    let mut entries = BTreeMap::new();
    entries.insert(name("readme"), Node::File(as_bytes));
    batch.stage_encoded(&Tree::new(entries))?;

    let error = journal.append(Seq(0), &batch).expect_err("prefix mismatch must fail");
    match error {
        AppendError::PrefixMismatch { digest, expected, actual } => {
            assert_eq!(digest, text.digest());
            assert_eq!(expected, OpaqueBytes::ID);
            assert_eq!(actual, Utf8Text::ID);
        }
        other => panic!("expected PrefixMismatch, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    Ok(())
}

#[test]
fn a_well_formed_tree_batch_round_trips_through_get() -> Result<(), Box<dyn Error>> {
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut batch = Batch::new();
    let file = batch.stage_bytes(b"hi");
    let child = Tree::empty();
    let child_ref = batch.stage_encoded(&child)?;
    let mut entries = BTreeMap::new();
    entries.insert(name("a"), Node::File(file));
    entries.insert(name("b"), Node::Directory(child_ref));
    let root = Tree::new(entries);
    let root_ref = batch.stage_encoded(&root)?;
    journal.append(Seq(0), &batch)?;
    assert_eq!(journal.get::<Tree>(&root_ref.digest())?, Some(root));
    Ok(())
}
