use std::ptr;
use std::sync::{Arc, Weak};

use aether_data::BlobReader;

use super::entry::remove_if_current;
use super::gauge::next_mark;
use super::{BlobStore, Index};

fn store() -> BlobStore {
    BlobStore::new().expect("spawn the reclaim thread")
}

fn boxed(bytes: &[u8]) -> Box<[u8]> {
    bytes.into()
}

/// Catches a dedup miss (equal bytes resident twice) or counting the
/// duplicate's bytes.
#[test]
fn equal_bytes_share_one_entry_counted_once() {
    let store = store();

    let first = store.check_in(boxed(b"closure member"));
    let second = store.check_in(boxed(b"closure member"));

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(store.resident_bytes(), b"closure member".len());
}

/// Catches a strong index keeping bytes alive, or a stale slot surviving the
/// entry's drop.
#[test]
fn dropping_every_reference_frees_the_entry_and_its_slot() {
    let store = store();

    let first = store.check_in(boxed(b"transient"));
    let second = Arc::clone(&first);
    let hash = first.hash();
    // Held across the drop so the old allocation's address cannot be reused
    // by the next check-in, which keeps the pointer comparison below honest.
    let old: Weak<_> = Arc::downgrade(&first);
    drop(first);
    drop(second);

    assert!(old.upgrade().is_none());
    assert_eq!(store.resident_bytes(), 0);
    assert!(!store.shared.lock_index().contains_key(&hash));

    let fresh = store.check_in(boxed(b"transient"));

    assert!(!ptr::eq(old.as_ptr(), Arc::as_ptr(&fresh)));
    assert_eq!(store.resident_bytes(), b"transient".len());
}

/// Catches ADR-0238 decision 7's race: an older entry's drop removing the
/// slot a newer entry with the same hash has already taken.
#[test]
fn an_older_entry_leaves_a_newer_slot_in_place() {
    let store = store();
    let older = store.check_in(boxed(b"older"));
    let newer = store.check_in(boxed(b"newer"));
    let hash = older.hash();

    // The state the race leaves: `hash`'s slot already points at another,
    // newer entry when the older entry's drop takes the lock.
    let mut index = Index::default();
    index.insert(hash, Arc::downgrade(&newer));
    remove_if_current(&mut index, hash, Arc::as_ptr(&older));

    assert!(index.get(&hash).is_some_and(|slot| ptr::eq(slot.as_ptr(), Arc::as_ptr(&newer))));

    remove_if_current(&mut index, hash, Arc::as_ptr(&newer));

    assert!(index.is_empty());
}

/// Catches a gauge that warns on every check-in, or never.
#[test]
fn the_gauge_advances_only_when_a_mark_is_crossed() {
    assert_eq!(next_mark(100, 99), None);
    assert_eq!(next_mark(100, 100), None);
    assert_eq!(next_mark(100, 101), Some(200));
    assert_eq!(next_mark(200, 150), None);
    assert_eq!(next_mark(200, 900), Some(1600));
}

/// Catches a `BlobEntry::read_at` offset bug, a mint that holds a `Weak`, or
/// a leaked entry: a checked-in value reads its bytes from an offset, keeps
/// its entry resident through a clone's drop, and frees it when the last
/// clone goes.
#[test]
fn a_shared_blob_reads_from_an_offset_and_stays_resident_until_the_last_clone_drops() {
    let store = store();

    let blob = store.check_in(boxed(b"checked in")).into_blob();
    let clone = blob.clone();
    drop(clone);

    let mut buf = [0; 8];
    assert_eq!(BlobReader::open(&blob).read_range(3, &mut buf), 7);
    assert_eq!(buf[..7], *b"cked in");
    assert_eq!(store.resident_bytes(), b"checked in".len());

    drop(blob);

    assert_eq!(store.resident_bytes(), 0);
}
