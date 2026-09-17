//! Public [`Heads`] publication: same type, cursor, and bindings.

use std::error::Error;

use aether_bloomery_kinds::{Digest, Entry, Head, Program, Ref, Seq, Tree};
use aether_bloomery_view::{Heads, Publish, View};
use aether_data::{Kind, Storage, StorageData};

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

#[test]
fn published_heads_round_trip_cursor_and_typed_bindings() -> Result<(), Box<dyn Error>> {
    // Bug: publication drops the cursor, remaps heads by name alone, or returns a
    // different public type than the fold.
    let program = digest_ref::<Program>(1);
    let tree = digest_ref::<Tree>(2);
    let mut heads = Heads::new();
    heads.advance(&[moved(1, "main", program)?, moved(2, "main", tree)?])?;

    let bytes = heads.encode()?;
    let restored = Heads::decode(&bytes)?;
    assert_eq!(restored, heads);
    assert_eq!(restored.cursor(), Seq(2));
    assert_eq!(restored.get(&Head::<Program>::new("main")), Some(program));
    assert_eq!(restored.get(&Head::<Tree>::new("main")), Some(tree));
    assert_eq!(Heads::NAME, "bloomery.view.heads");
    assert_eq!(Heads::decode_from_bytes(&bytes).as_ref(), Some(&restored));
    assert_eq!(restored.encode_into_bytes(), bytes);
    Ok(())
}

#[test]
fn later_folds_do_not_change_a_decoded_snapshot() -> Result<(), Box<dyn Error>> {
    // Bug: a published snapshot aliases the live fold, so a later move mutates it.
    let first = digest_ref::<Program>(1);
    let second = digest_ref::<Program>(2);
    let head = Head::<Program>::new("main");
    let mut live = Heads::new();
    live.advance(&[moved(1, "main", first)?])?;
    let owned = Heads::decode(&live.encode()?)?;

    live.advance(&[moved(2, "main", second)?])?;
    assert_eq!(owned.cursor(), Seq(1));
    assert_eq!(owned.get(&head), Some(first));
    assert_eq!(live.cursor(), Seq(2));
    assert_eq!(live.get(&head), Some(second));
    Ok(())
}

#[test]
fn malformed_published_bytes_are_refused() -> Result<(), Box<dyn Error>> {
    // Bug: truncated or trailing snapshot bytes still construct a Heads value.
    let mut heads = Heads::new();
    heads.advance(&[moved(1, "main", digest_ref::<Program>(1))?])?;
    let bytes = heads.encode()?;

    assert!(Heads::decode(&[]).is_err());
    assert!(Heads::decode(&bytes[..bytes.len().saturating_sub(1)]).is_err());
    let mut trailing = bytes;
    trailing.push(0xff);
    assert!(Heads::decode(&trailing).is_err());
    assert!(Heads::decode_from_bytes(&trailing).is_none());
    Ok(())
}
