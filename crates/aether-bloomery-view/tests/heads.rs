//! Fold semantics of [`Heads`]: last move per `(KindId, name)` on a contiguous prefix.

use std::error::Error;

use aether_bloomery_kinds::{
    Digest, Entry, Head, Mode, OpaqueBytes, Program, ProgramHeadMoved, ProgramName, RecordedHead, RecordedHeadMove,
    Ref, Seq, Tree,
};
use aether_bloomery_view::{HeadFoldError, Heads, SequenceError, View};
use aether_data::{Kind, Storage, StorageData};

const PAGE: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.note")]
struct Note {
    n: u64,
}

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn entry_for<K: Storage + Clone>(seq: u64, event: &K) -> Result<Entry, Box<dyn Error>> {
    Ok(Entry {
        seq: Seq(seq),
        kind: K::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(event.clone()))?,
    })
}

fn moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &Head::<K>::new(name).move_to(to))
}

fn historical(seq: u64, name: &str, program: Ref<Program>) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &ProgramHeadMoved { name: ProgramName::new(name)?, program })
}

fn note(seq: u64, n: u64) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &Note { n })
}

fn malformed(seq: u64, kind: aether_data::KindId) -> Entry {
    Entry { seq: Seq(seq), kind, cause: None, recorded_at_millis: 0, bytes: vec![0xff] }
}

fn program(name: &str, intent: &str) -> Result<Program, Box<dyn Error>> {
    Ok(Program {
        name: ProgramName::new(name)?,
        input: OpaqueBytes::ID,
        result: OpaqueBytes::ID,
        mode: Mode::Pure,
        intent: intent.into(),
    })
}

#[test]
fn same_name_under_two_kinds_stays_independent() -> Result<(), Box<dyn Error>> {
    // Bug: bindings keyed only by name, so a program head and a tree head named "main" overwrite each other.
    let program_ref = digest_ref::<Program>(1);
    let tree_ref = digest_ref::<Tree>(2);
    let mut heads = Heads::new();
    heads.apply(&moved(1, "main", program_ref)?)?;
    heads.apply(&moved(2, "main", tree_ref)?)?;

    let program_head = Head::<Program>::new("main");
    let tree_head = Head::<Tree>::new("main");
    assert_eq!(heads.get(&program_head), Some(program_ref));
    assert_eq!(heads.get(&tree_head), Some(tree_ref));
    Ok(())
}

#[test]
fn head_name_is_independent_of_target_label() -> Result<(), Box<dyn Error>> {
    // Bug: the fold keys a program head by the declaration's ProgramName instead of the move's Head.
    let stored = Ref::of_encoded(&program("trim", "strip")?)?;
    let mut heads = Heads::new();
    heads.apply(&moved(1, "main", stored)?)?;

    assert_eq!(heads.get(&Head::<Program>::new("main")), Some(stored));
    assert_eq!(heads.get(&Head::<Program>::new("trim")), None);
    Ok(())
}

#[test]
fn first_reassign_repeat_and_return_are_exact_at_each_cursor() -> Result<(), Box<dyn Error>> {
    // Bug: first-write-wins, skipped identical reassignment, or a forbidden return to an older target.
    let first = digest_ref::<Program>(1);
    let second = digest_ref::<Program>(2);
    let head = Head::<Program>::new("trim");
    let mut heads = Heads::new();

    heads.apply(&moved(1, "trim", first)?)?;
    assert_eq!(heads.cursor(), Seq(1));
    assert_eq!(heads.get(&head), Some(first));

    heads.apply(&moved(2, "trim", second)?)?;
    assert_eq!(heads.cursor(), Seq(2));
    assert_eq!(heads.get(&head), Some(second));

    heads.apply(&moved(3, "trim", second)?)?;
    assert_eq!(heads.cursor(), Seq(3));
    assert_eq!(heads.get(&head), Some(second));

    heads.apply(&moved(4, "trim", first)?)?;
    assert_eq!(heads.cursor(), Seq(4));
    assert_eq!(heads.get(&head), Some(first));
    Ok(())
}

#[test]
fn mixed_historical_and_generic_program_moves_use_sequence_order() -> Result<(), Box<dyn Error>> {
    // Bug: one kind always wins, or historical rows are rewritten as generic events, so order is not seq order.
    let older = digest_ref::<Program>(1);
    let newer = digest_ref::<Program>(2);
    let head = Head::<Program>::new("trim");

    let mut historical_then_generic = Heads::new();
    historical_then_generic.apply(&historical(1, "trim", older)?)?;
    historical_then_generic.apply(&moved(2, "trim", newer)?)?;
    assert_eq!(historical_then_generic.get(&head), Some(newer));
    assert_eq!(historical_then_generic.cursor(), Seq(2));

    let mut generic_then_historical = Heads::new();
    generic_then_historical.apply(&moved(1, "trim", newer)?)?;
    generic_then_historical.apply(&historical(2, "trim", older)?)?;
    assert_eq!(generic_then_historical.get(&head), Some(older));
    assert_eq!(generic_then_historical.cursor(), Seq(2));
    Ok(())
}

#[test]
fn ignored_entries_advance_the_cursor() -> Result<(), Box<dyn Error>> {
    // Bug: unrelated kinds are skipped without advancing, so the next real seq looks like a gap.
    let to = digest_ref::<Program>(1);
    let head = Head::<Program>::new("trim");
    let mut heads = Heads::new();
    heads.apply(&moved(1, "trim", to)?)?;
    heads.apply(&note(2, 7)?)?;

    assert_eq!(heads.cursor(), Seq(2));
    assert_eq!(heads.get(&head), Some(to));
    Ok(())
}

#[test]
fn rejected_input_leaves_cursor_and_bindings_unchanged() -> Result<(), Box<dyn Error>> {
    // Bug: a refused apply still writes the binding or cursor, so a gap or duplicate corrupts the prefix.
    let to = digest_ref::<Program>(1);
    let later = digest_ref::<Program>(2);
    let head = Head::<Program>::new("trim");
    let mut heads = Heads::new();
    heads.apply(&moved(1, "trim", to)?)?;

    let gap = heads.apply(&moved(3, "trim", later)?).expect_err("gap must refuse");
    match gap {
        HeadFoldError::Sequence(SequenceError::Gap { expected, actual }) => {
            assert_eq!(expected, Seq(2));
            assert_eq!(actual, Seq(3));
        }
        other => panic!("expected Gap, got {other:?}"),
    }

    let duplicate = heads.apply(&moved(1, "trim", later)?).expect_err("duplicate must refuse");
    match duplicate {
        HeadFoldError::Sequence(SequenceError::Duplicate { expected, actual }) => {
            assert_eq!(expected, Seq(2));
            assert_eq!(actual, Seq(1));
        }
        other => panic!("expected Duplicate, got {other:?}"),
    }

    heads.apply(&moved(2, "trim", to)?)?;
    let backwards = heads.apply(&moved(1, "trim", later)?).expect_err("backwards must refuse");
    match backwards {
        HeadFoldError::Sequence(SequenceError::Backwards { expected, actual }) => {
            assert_eq!(expected, Seq(3));
            assert_eq!(actual, Seq(1));
        }
        other => panic!("expected Backwards, got {other:?}"),
    }

    assert_eq!(heads.cursor(), Seq(2));
    assert_eq!(heads.get(&head), Some(to));
    Ok(())
}

#[test]
fn malformed_historical_and_generic_events_fail_visibly() {
    // Bug: a recognized kind with undecodable bytes is ignored, so the cursor claims a prefix that was not read.
    let mut generic = Heads::new();
    let generic_error = generic.apply(&malformed(1, RecordedHeadMove::ID)).expect_err("malformed generic");
    assert!(matches!(generic_error, HeadFoldError::Decode(_)), "{generic_error:?}");
    assert_eq!(generic.cursor(), Seq(0));
    assert_eq!(generic.get(&Head::<Program>::new("trim")), None);

    let mut historical_heads = Heads::new();
    let historical_error =
        historical_heads.apply(&malformed(1, ProgramHeadMoved::ID)).expect_err("malformed historical");
    assert!(matches!(historical_error, HeadFoldError::Decode(_)), "{historical_error:?}");
    assert_eq!(historical_heads.cursor(), Seq(0));
}

#[test]
fn incremental_fold_across_chunks_matches_rebuild_from_zero() -> Result<(), Box<dyn Error>> {
    // Bug: last-write-wins is applied per chunk, or incremental apply diverges from a fresh fold of the same prefix.
    let filler = digest_ref::<Program>(9);
    let first = digest_ref::<Program>(1);
    let second = digest_ref::<Program>(2);
    let hash_to = digest_ref::<Program>(3);
    let mut entries = Vec::new();
    for index in 0..PAGE {
        let name = format!("f{index:03}");
        let seq = u64::try_from(index)? + 1;
        entries.push(entry_for(seq, &RecordedHeadMove::new(RecordedHead::new(Program::ID, name)?, filler.digest()))?);
    }
    let after_fill = u64::try_from(PAGE)? + 1;
    entries.push(moved(after_fill, "trim", first)?);
    entries.push(moved(after_fill + 1, "trim", second)?);
    entries.push(moved(after_fill + 2, "hash", hash_to)?);

    let mut chunked = Heads::new();
    for chunk in entries.chunks(PAGE) {
        chunked.advance(chunk)?;
    }
    let mut rebuilt = Heads::new();
    rebuilt.advance(&entries)?;

    let trim = Head::<Program>::new("trim");
    let hash = Head::<Program>::new("hash");
    let fill = Head::<Program>::new("f000");
    assert_eq!(chunked.cursor(), Seq(u64::try_from(PAGE)? + 3));
    assert_eq!(chunked.get(&trim), Some(second));
    assert_eq!(chunked.get(&hash), Some(hash_to));
    assert_eq!(chunked.get(&fill), Some(filler));
    assert_eq!(chunked.cursor(), rebuilt.cursor());
    assert_eq!(chunked.get(&trim), rebuilt.get(&trim));
    assert_eq!(chunked.get(&hash), rebuilt.get(&hash));
    assert_eq!(chunked.get(&fill), rebuilt.get(&fill));
    Ok(())
}

#[test]
fn recorded_lookup_keeps_kind_and_name_apart() -> Result<(), Box<dyn Error>> {
    // Catches a lookup keyed on the name alone, so a tree head answers for a same-named bundle head.
    let bundle = Digest::from_bytes([1; 32]);
    let tree = Digest::from_bytes([2; 32]);
    let mut heads = Heads::new();
    heads.apply(&moved(1, "main", Ref::<OpaqueBytes>::from_digest(bundle))?)?;
    heads.apply(&moved(2, "main", Ref::<Tree>::from_digest(tree))?)?;

    let bundle_head = RecordedHead::new(OpaqueBytes::ID, "main")?;
    let tree_head = RecordedHead::new(Tree::ID, "main")?;
    assert_eq!(heads.binding(&bundle_head), Some(bundle));
    assert_eq!(heads.binding(&tree_head), Some(tree));
    assert_eq!(heads.binding(&RecordedHead::new(Program::ID, "main")?), None);
    Ok(())
}
