//! Append validates `bloomery.head_moved` destinations atomically with the batch.

mod common;

use std::error::Error;

use aether_bloomery_journal::{AppendError, Batch, Digest, Draft, Journal, OpaqueBytes, Seq, Utf8Text};
use aether_bloomery_kinds::{Head, HeadMoved, RecordedHeadMove, Ref, Tree};
use aether_data::{Kind, KindId, Storage, StorageData};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.head_note")]
struct Note {
    text: String,
}

/// Same kind name as [`RecordedHeadMove`], with a raw `head` so invalid bytes can be encoded.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.head_moved")]
struct RawHeadMoved {
    target_kind: KindId,
    head: String,
    to: Digest,
}

/// Same kind name as [`RecordedHeadMove`], but `to` walks as `Ref<OpaqueBytes>`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.head_moved")]
struct CitedHeadMoved {
    target_kind: KindId,
    head: String,
    to: Ref<OpaqueBytes>,
}

fn tree_head(name: &'static str) -> Head<Tree> {
    Head::<Tree>::new(name)
}

fn move_event<K: Kind + 'static>(head: &Head<K>, target: Ref<K>) -> HeadMoved<K> {
    head.move_to(target)
}

#[test]
fn a_move_to_an_already_stored_target_appends() -> Result<(), Box<dyn Error>> {
    // Catches a verifier that only inspects the current batch's staged set, so a
    // target committed earlier is treated as missing.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut setup = Batch::new();
    let tree = setup.stage_encoded(&Tree::empty())?;
    setup.push_event(&Note { text: "keep".into() }, None)?;
    journal.append(Seq(0), &setup)?;

    let mut batch = Batch::new();
    let main = Head::<Tree>::new("main");
    let event: HeadMoved<Tree> = main.move_to(tree);
    batch.push_event(&event, None)?;
    let range = journal.append(Seq(1), &batch)?;
    assert_eq!(range, Seq(2)..Seq(3));

    let entry = journal.read(Seq(1), 1)?.into_iter().next().expect("one move");
    let event = Journal::decode::<HeadMoved<Tree>>(&entry)?;
    assert_eq!(event.head().kind(), Tree::ID);
    assert_eq!(event.head().as_str(), "main");
    assert_eq!(event.to(), tree);
    Ok(())
}

#[test]
fn a_move_to_a_same_batch_target_appends() -> Result<(), Box<dyn Error>> {
    // Catches a check that runs before insert_staged, or against the
    // pre-transaction store, so a new target has to predate the move.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut batch = Batch::new();
    let tree = batch.stage_encoded(&Tree::empty())?;
    batch.push_event(&move_event(&tree_head("main"), tree), None)?;
    journal.append(Seq(0), &batch)?;

    let entry = journal.read(Seq(0), 1)?.into_iter().next().expect("one move");
    assert_eq!(Journal::decode::<HeadMoved<Tree>>(&entry)?.to(), tree);
    assert_eq!(journal.get::<Tree>(&tree.digest())?, Some(Tree::empty()));
    Ok(())
}

#[test]
fn a_missing_head_target_rolls_back_the_whole_batch() -> Result<(), Box<dyn Error>> {
    // Catches trusting Draft citation metadata (Digest walks nothing) or
    // insert-then-verify with no rollback, which would leave the sibling blob
    // and note in the store.
    let (_root, mut journal) = common::temp_journal(0)?;
    let missing = Ref::<Tree>::from_digest(Digest::from_bytes([7; 32]));
    let mut batch = Batch::new();
    let staged = batch.stage_bytes(b"should-not-land");
    batch.push_event(&Note { text: "also-not-land".into() }, None)?;
    batch.push_event(&move_event(&tree_head("main"), missing), None)?;

    let error = journal.append(Seq(0), &batch).expect_err("missing head target must fail");
    match error {
        AppendError::DanglingRef { digest, expected } => {
            assert_eq!(digest, missing.digest());
            assert_eq!(expected, Tree::ID);
        }
        other => panic!("expected DanglingRef, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    assert!(journal.read(Seq(0), 16)?.is_empty());
    assert_eq!(journal.get_bytes(&staged.digest())?, None);
    Ok(())
}

#[test]
fn a_wrong_kind_head_target_rolls_back_the_whole_batch() -> Result<(), Box<dyn Error>> {
    // Catches a destination check that tests existence only, so a text blob
    // would be accepted as a tree head.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut batch = Batch::new();
    let text = batch.stage_text("hello");
    let as_tree = Ref::<Tree>::from_digest(text.digest());
    batch.push_event(&Note { text: "also-not-land".into() }, None)?;
    batch.push_event(&move_event(&tree_head("main"), as_tree), None)?;

    let error = journal.append(Seq(0), &batch).expect_err("wrong-kind head target must fail");
    match error {
        AppendError::PrefixMismatch { digest, expected, actual } => {
            assert_eq!(digest, text.digest());
            assert_eq!(expected, Tree::ID);
            assert_eq!(actual, Utf8Text::ID);
        }
        other => panic!("expected PrefixMismatch, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    assert!(journal.read(Seq(0), 16)?.is_empty());
    assert_eq!(journal.get_bytes(&text.digest())?, None);
    Ok(())
}

#[test]
fn push_event_and_push_draft_both_refuse_a_malformed_head_moved_payload() -> Result<(), Box<dyn Error>> {
    // Catches a boundary that trusts the original Rust type or kind name and
    // skips canonical decode, so invalid head-name bytes would land.
    let twin = RawHeadMoved { target_kind: Tree::ID, head: "has space".into(), to: Digest::from_bytes([1; 32]) };
    let bytes = RawHeadMoved::encode_storage(&StorageData::from_value(twin.clone()))?;
    let decode_error =
        RecordedHeadMove::decode_storage(&bytes).expect_err("canonical decode must refuse invalid head-name bytes");

    let (_root, mut journal) = common::temp_journal(0)?;
    let mut via_event = Batch::new();
    via_event.push_event(&twin, None)?;
    match journal.append(Seq(0), &via_event).expect_err("malformed push_event must fail") {
        AppendError::InvalidHeadMoved(error) => assert_eq!(error, decode_error),
        other => panic!("expected InvalidHeadMoved from push_event, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));

    let mut via_draft = Batch::new();
    via_draft.push_draft(Draft::of(&twin, None)?);
    match journal.append(Seq(0), &via_draft).expect_err("malformed push_draft must fail") {
        AppendError::InvalidHeadMoved(error) => assert_eq!(error, decode_error),
        other => panic!("expected InvalidHeadMoved from push_draft, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    Ok(())
}

#[test]
fn push_event_and_push_draft_both_refuse_a_wrong_kind_head_destination() -> Result<(), Box<dyn Error>> {
    // Catches an entry path that skips the recognized-event check, and a
    // verifier that trusts draft citation metadata: the twin walks a valid
    // opaque-bytes blob while the encoded target_kind is Tree.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut via_event = Batch::new();
    let text = via_event.stage_text("hello");
    let as_tree = Ref::<Tree>::from_digest(text.digest());
    via_event.push_event(&move_event(&tree_head("main"), as_tree), None)?;
    match journal.append(Seq(0), &via_event).expect_err("push_event wrong kind must fail") {
        AppendError::PrefixMismatch { digest, expected, actual } => {
            assert_eq!(digest, text.digest());
            assert_eq!(expected, Tree::ID);
            assert_eq!(actual, Utf8Text::ID);
        }
        other => panic!("expected PrefixMismatch from push_event, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    assert_eq!(journal.get_bytes(&text.digest())?, None);

    let mut via_draft = Batch::new();
    let bytes = via_draft.stage_bytes(b"hello");
    let twin = CitedHeadMoved { target_kind: Tree::ID, head: "main".into(), to: Ref::from_digest(bytes.digest()) };
    via_draft.push_draft(Draft::of(&twin, None)?);
    match journal.append(Seq(0), &via_draft).expect_err("push_draft wrong kind must fail") {
        AppendError::PrefixMismatch { digest, expected, actual } => {
            assert_eq!(digest, bytes.digest());
            assert_eq!(expected, Tree::ID);
            assert_eq!(actual, OpaqueBytes::ID);
        }
        other => panic!("expected PrefixMismatch from push_draft, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(0));
    assert_eq!(journal.get_bytes(&bytes.digest())?, None);
    Ok(())
}

#[test]
fn a_stale_fence_writes_nothing_for_a_valid_move() -> Result<(), Box<dyn Error>> {
    // Catches skipping the global sequence fence for a recognized head-move,
    // or writing the move after returning AppendError::HeadMoved.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut setup = Batch::new();
    let tree = setup.stage_encoded(&Tree::empty())?;
    setup.push_event(&Note { text: "kept".into() }, None)?;
    journal.append(Seq(0), &setup)?;
    let before = journal.read(Seq(0), 16)?;

    let mut batch = Batch::new();
    let stray = batch.stage_bytes(b"should-not-land");
    batch.push_event(&move_event(&tree_head("main"), tree), None)?;
    let error = journal.append(Seq(0), &batch).expect_err("stale fence must fail");
    match error {
        AppendError::HeadMoved { actual } => assert_eq!(actual, Seq(1)),
        other => panic!("expected HeadMoved, got {other:?}"),
    }
    assert_eq!(journal.head()?, Seq(1));
    assert_eq!(journal.read(Seq(0), 16)?, before);
    assert_eq!(journal.get_bytes(&stray.digest())?, None);
    Ok(())
}

#[test]
fn multiple_moves_replay_and_repeat_remain_distinct_ordered_events() -> Result<(), Box<dyn Error>> {
    // Catches hidden deduplication or last-wins collapsing at append time.
    // A move back to an old target and a repeated identical assignment are
    // still recorded in order.
    let (_root, mut journal) = common::temp_journal(0)?;
    let mut batch = Batch::new();
    let first = batch.stage_bytes(b"first");
    let second = batch.stage_bytes(b"second");
    let head = Head::<OpaqueBytes>::new("main");
    batch.push_event(&move_event(&head, first), None)?;
    batch.push_event(&move_event(&head, second), None)?;
    batch.push_event(&move_event(&head, first), None)?;
    batch.push_event(&move_event(&head, first), None)?;
    let range = journal.append(Seq(0), &batch)?;
    assert_eq!(range, Seq(1)..Seq(5));

    let entries = journal.read(Seq(0), 8)?;
    assert_eq!(entries.len(), 4);
    let expected = [first.digest(), second.digest(), first.digest(), first.digest()];
    for (index, (entry, digest)) in entries.iter().zip(expected).enumerate() {
        assert_eq!(entry.seq, Seq(u64::try_from(index + 1)?));
        assert_eq!(entry.kind, RecordedHeadMove::ID);
        let event = Journal::decode::<HeadMoved<OpaqueBytes>>(entry)?;
        assert_eq!(event.head().as_str(), "main");
        assert_eq!(event.head().kind(), OpaqueBytes::ID);
        assert_eq!(event.to().digest(), digest);
    }
    Ok(())
}
