//! Portable `Heads` fold over supplied entries without the native journal store.

use std::error::Error;

use aether_bloomery_kinds::{Digest, Entry, Head, Program, Ref, Seq, Tree};
use aether_bloomery_view::{Heads, View};
use aether_data::{Kind, Storage, StorageData};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.portable_note")]
struct Note {
    n: u64,
}

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn entry_for<K: Storage + Clone>(seq: u64, event: &K) -> Result<Entry, Box<dyn Error>> {
    Ok(Entry {
        seq: Seq(seq),
        kind: K::NAME.to_owned(),
        cause: None,
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(event.clone()))?,
    })
}

fn moved<K: Kind + 'static>(seq: u64, name: &'static str, to: Ref<K>) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &Head::<K>::new(name).move_to(to))
}

fn note(seq: u64, n: u64) -> Result<Entry, Box<dyn Error>> {
    entry_for(seq, &Note { n })
}

#[test]
fn contiguous_head_move_and_irrelevant_entries_lookup_typed_heads() -> Result<(), Box<dyn Error>> {
    // Bug: supplied-entry fold cannot decode entries, skips ignored kinds
    // without advancing, or looks up heads by name alone.
    let first = digest_ref::<Program>(1);
    let second = digest_ref::<Program>(2);
    let tree = digest_ref::<Tree>(3);
    let program_head = Head::<Program>::new("trim");
    let tree_head = Head::<Tree>::new("trim");
    let mut heads = Heads::new();

    heads.advance(&[moved(1, "trim", first)?, note(2, 9)?, moved(3, "trim", second)?, moved(4, "trim", tree)?])?;

    assert_eq!(heads.cursor(), Seq(4));
    assert_eq!(heads.get(&program_head), Some(second));
    assert_eq!(heads.get(&tree_head), Some(tree));
    assert_eq!(heads.get(&Head::<Program>::new("other")), None);
    Ok(())
}
