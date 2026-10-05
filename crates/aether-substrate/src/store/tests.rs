use std::ptr;
use std::sync::{Arc, Weak};

use aether_data::BlobReader;

use super::entry::remove_if_current;
use super::gauge::next_mark;
use super::{BlobEntry, BlobStore, Index};

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

/// A slab over `members`, each region filled with its member's bytes.
fn slab_of(store: &BlobStore, members: &[&[u8]]) -> Vec<Arc<BlobEntry>> {
    let lens: Vec<_> = members.iter().map(|member| member.len()).collect();
    let mut slab = store.slab(&lens);
    slab.regions().zip(members).for_each(|(region, member)| region.copy_from_slice(member));
    slab.finish()
}

/// Catches range bookkeeping that hands a member its neighbour's bytes, an
/// empty region that panics or shifts the ones after it, and a slab member
/// that fails to reuse a resident entry.
#[test]
fn slab_members_read_their_own_regions_and_reuse_resident_entries() {
    let store = store();
    let resident = store.check_in(boxed(b"resident"));
    let members: [&[u8]; 4] = [b"first", b"", b"resident", b"second"];

    let entries = slab_of(&store, &members);

    assert_eq!(entries.iter().map(|entry| entry.bytes()).collect::<Vec<_>>(), members);
    assert!(Arc::ptr_eq(&entries[2], &resident));
}

/// Catches a missing `contiguous` override (the default `None` would make
/// render refuse every store-shared asset) and a slab member answering with
/// the whole slab or a neighbour's region.
#[test]
fn store_entries_answer_contiguous_with_exactly_their_own_bytes() {
    let store = store();
    let owned = store.check_in(boxed(b"owned bytes")).into_blob();
    let members: [&[u8]; 3] = [b"first", b"", b"second"];

    let slab = slab_of(&store, &members).into_iter().map(BlobEntry::into_blob).collect::<Vec<_>>();

    assert_eq!(owned.contiguous(), Some(b"owned bytes".as_slice()));
    assert_eq!(slab.iter().map(|blob| blob.contiguous()).collect::<Vec<_>>(), members.map(Some));
}

/// Catches an owned check-in that adopts a slab entry, and a displaced slab
/// entry whose drop removes the owned entry's slot.
#[test]
fn an_owned_check_in_takes_the_dedup_slot_from_a_slab_entry() {
    let store = store();
    let member = slab_of(&store, &[b"sharing".as_slice()]).pop().expect("one member");
    let hash = member.hash();

    let owned = store.check_in(boxed(b"sharing"));

    assert!(!Arc::ptr_eq(&owned, &member));
    assert!(Arc::ptr_eq(&store.check_in(boxed(b"sharing")), &owned));
    assert_eq!(member.bytes(), b"sharing");

    drop(member);

    assert!(store.shared.lock_index().get(&hash).is_some_and(|slot| ptr::eq(slot.as_ptr(), Arc::as_ptr(&owned))));
}

/// Catches per-member subtraction from `resident_bytes` (a double count or an
/// underflow), a dedup-hit region counted as a live member, and a leaked slab.
#[test]
fn slab_bytes_stay_resident_until_the_last_member_drops_and_retention_is_reported() {
    let store = store();
    let resident = store.check_in(boxed(b"resident"));
    let counts = |store: &BlobStore| (store.resident_bytes(), store.slab_bytes(), store.slab_member_bytes());
    let before = counts(&store);

    let mut entries = slab_of(&store, &[b"resident".as_slice(), b"one", b"four"]).into_iter();
    let (hit, one, four) = (entries.next(), entries.next(), entries.next());

    assert!(hit.as_ref().is_some_and(|hit| Arc::ptr_eq(hit, &resident)));
    assert_eq!(counts(&store), (8 + 15, 15, 7));

    drop(hit);
    drop(one);

    assert_eq!(counts(&store), (8 + 15, 15, 4));

    drop(four);

    assert_eq!(counts(&store), before);
}

/// Catches a leak on the journal's mid-read error path: a builder dropped
/// before `finish` must free its slab and leave no count or slot behind.
#[test]
fn an_unfinished_slab_is_freed_and_uncounted() {
    let store = store();
    let mut slab = store.slab(&[4, 6]);
    slab.regions().next().expect("a first region").copy_from_slice(b"half");

    assert_eq!((store.resident_bytes(), store.slab_bytes()), (10, 10));

    drop(slab);

    assert_eq!((store.resident_bytes(), store.slab_bytes(), store.slab_member_bytes()), (0, 0, 0));
    assert!(store.shared.lock_index().is_empty());
}

/// Catches a view that fails to hold its root (the buffer freed under a live
/// view), a view's bytes counted a second time, and a root that stays
/// resident after its value and its last view are gone, in either drop order.
#[test]
fn a_view_holds_its_root_resident_and_counts_no_bytes_of_its_own() {
    let store = store();
    let file: &[u8] = b"module file bytes";

    let root = store.check_in(boxed(file));
    let view = store.view(&root, 7..11).expect("the range is in bounds");
    drop(root);

    assert_eq!(view.bytes(), b"file");
    assert_eq!(store.resident_bytes(), file.len());

    drop(view);

    assert_eq!(store.resident_bytes(), 0);
    assert!(store.shared.lock_index().is_empty());

    let root = store.check_in(boxed(file));
    let view = store.view(&root, 7..11).expect("the range is in bounds");
    drop(view);

    assert_eq!(store.resident_bytes(), file.len());

    drop(root);

    assert_eq!(store.resident_bytes(), 0);
    assert!(store.shared.lock_index().is_empty());
}

/// Catches an off-by-range rebase, a view of a view that holds the
/// intermediate view rather than the root, and one whose range is checked
/// against the root, so it could read past the view it was taken from.
#[test]
fn a_view_of_a_view_reads_the_rebased_range_of_the_root() {
    let store = store();
    let root = store.check_in(boxed(b"0123456789abcdef"));
    let outer = store.view(&root, 4..12).expect("the range is in bounds");

    let inner = store.view(&outer, 2..5).expect("the range is in bounds");

    assert_eq!(outer.bytes(), b"456789ab");
    assert_eq!(inner.bytes(), b"678");
    assert!(store.view(&outer, 6..9).is_none(), "a range past the view's end is refused though the root has it");

    drop(outer);
    drop(root);

    assert_eq!(inner.bytes(), b"678");
    assert_eq!(store.resident_bytes(), 16);
}

/// Catches a second entry for bytes that are already resident, and a range
/// outside the root accepted.
#[test]
fn a_view_reuses_a_resident_entry_and_refuses_a_range_outside_its_root() {
    let store = store();
    let resident = store.check_in(boxed(b"file"));
    let root = store.check_in(boxed(b"module file bytes"));

    let view = store.view(&root, 7..11).expect("the range is in bounds");

    assert!(Arc::ptr_eq(&view, &resident));
    assert!(Arc::ptr_eq(&store.view(&root, 0..17).expect("the whole root"), &root));
    assert!(store.view(&root, 11..18).is_none());
}

/// Catches an owned check-in that adopts a view, which would keep the view's
/// whole root resident for a holder that never chose it, and a displaced
/// view whose drop removes the owned entry's slot.
#[test]
fn an_owned_check_in_takes_the_dedup_slot_from_a_view() {
    let store = store();
    let root = store.check_in(boxed(b"module file bytes"));
    let view = store.view(&root, 7..11).expect("the range is in bounds");
    let hash = view.hash();

    let owned = store.check_in(boxed(b"file"));

    assert!(!Arc::ptr_eq(&owned, &view));
    assert_eq!(owned.hash(), hash);

    drop(view);
    drop(root);

    assert_eq!(store.resident_bytes(), b"file".len());
    assert!(store.shared.lock_index().get(&hash).is_some_and(|slot| ptr::eq(slot.as_ptr(), Arc::as_ptr(&owned))));
}
