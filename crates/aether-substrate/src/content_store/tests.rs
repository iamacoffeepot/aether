//! Domain-clean core tests: content addressing, dedup, persistence,
//! pinning, the eviction-policy parameter, and what two live handles on
//! one root can see of each other. These exercise the storage primitive
//! over a trivial metadata type `M` — the hub's binary / component
//! projections are tested against the real manifests in `aether-fleet`.

use super::{ContentStore, EvictionPolicy, Selector, hash_hex, now_nanos};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::{env, fs, process};

/// A trivial sidecar metadata type standing in for a real domain manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    label: String,
}

fn meta(label: &str) -> Meta {
    Meta { label: label.to_owned() }
}

fn temp_root(label: &str) -> PathBuf {
    env::temp_dir().join(format!("aether-content-store-test-{label}-{}-{}", process::id(), now_nanos()))
}

#[test]
fn upload_dedups_identical_bytes_to_one_hash() {
    let root = temp_root("dedup");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let h1 = store.upload(b"the-bytes", meta("a"), None).expect("upload lands");
    let h2 = store.upload(b"the-bytes", meta("a"), None).expect("upload lands");
    assert_eq!(h1, h2, "identical bytes dedup to the same content hash");
    assert_eq!(store.entry_count(), 1, "dedup stores one entry");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn name_repoints_to_the_latest_uploaded_hash() {
    let root = temp_root("repoint");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let h_old = store.upload(b"v1", meta("a"), Some("svc".to_owned())).expect("upload lands");
    let h_new = store.upload(b"v2", meta("a"), Some("svc".to_owned())).expect("upload lands");
    assert_ne!(h_old, h_new);
    let resolved = store.get(&Selector::Name("svc".to_owned())).expect("the name resolves");
    assert_eq!(resolved.hash, h_new, "the name points at the latest upload");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn entries_and_metadata_persist_across_a_reopen() {
    let root = temp_root("persist");
    let hash = {
        let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
        store.upload(b"persisted-bytes", meta("keep"), Some("svc".to_owned())).expect("upload lands")
        // store drops here — LockGuard releases lock.pid
    };
    let mut reopened: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    assert!(reopened.contains(&hash), "the entry survives a reopen");
    let resolved = reopened.get(&Selector::Name("svc".to_owned())).expect("the name survives a reopen");
    assert_eq!(resolved.hash, hash);
    assert_eq!(resolved.metadata, meta("keep"), "the sidecar metadata restores");
    let bytes = fs::read(&resolved.path).expect("the stored bytes are readable");
    assert_eq!(bytes, b"persisted-bytes");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn lru_budget_evicts_the_oldest_unnamed_unpinned_entry() {
    let root = temp_root("evict");
    // Budget holds the three ~10-byte initial entries (≈31 bytes) but
    // not a fourth, so the trigger upload forces exactly one eviction —
    // of the only unnamed, unpinned candidate.
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(40)).expect("open store");
    let h_plain = store.upload(b"plain-aaaa", meta("a"), None).expect("upload lands");
    let h_named = store.upload(b"named-bbbb", meta("a"), Some("keep".to_owned())).expect("upload lands");
    let h_pinned = store.upload(b"pinned-ccc", meta("a"), None).expect("upload lands");
    assert!(store.pin(&h_pinned), "pin targets a stored entry");
    let _ = store.upload(b"trigger-ddd", meta("a"), None).expect("upload lands");

    assert!(store.contains(&h_named), "a named entry is never evicted");
    assert!(store.contains(&h_pinned), "a pinned entry is never evicted");
    assert!(!store.contains(&h_plain), "the oldest unnamed, unpinned entry is evicted first");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: the eviction-policy parameter is load-bearing, not cosmetic.
/// The identical over-budget upload sequence evicts an unnamed, unpinned
/// entry under `LruBudget` but retains it under `None` — the eviction-free
/// canonical-record guarantee ADR-0149's `artifacts` port depends on. If
/// `evict_if_needed` stopped honoring the policy, the `None` assertion
/// would fail.
#[test]
fn eviction_free_policy_retains_what_lru_would_reclaim() {
    // Same tiny budget-shaped byte sizes as the LRU test, but the
    // eviction-free store never evicts regardless of the ledger.
    let lru_root = temp_root("policy-lru");
    let mut lru: ContentStore<Meta> = ContentStore::open(&lru_root, EvictionPolicy::LruBudget(40)).expect("open store");
    let free_root = temp_root("policy-free");
    let mut free: ContentStore<Meta> = ContentStore::open(&free_root, EvictionPolicy::None).expect("open store");

    let mut lru_plain = String::new();
    let mut free_plain = String::new();
    for store in [&mut lru, &mut free] {
        // The unnamed, unpinned first entry is the eviction candidate.
        let plain = store.upload(b"plain-aaaa", meta("a"), None).expect("upload lands");
        store.upload(b"named-bbbb", meta("a"), Some("keep".to_owned())).expect("upload lands");
        store.upload(b"pinned-ccc", meta("a"), None).expect("upload lands");
        // Trigger an over-budget upload.
        store.upload(b"trigger-ddd", meta("a"), None).expect("upload lands");
        if lru_plain.is_empty() {
            lru_plain = plain;
        } else {
            free_plain = plain;
        }
    }

    assert!(!lru.contains(&lru_plain), "the LRU policy reclaims the unnamed, unpinned entry");
    assert!(free.contains(&free_plain), "the eviction-free policy retains the same entry");
    assert_eq!(free.entry_count(), 4, "the eviction-free store keeps every entry");
    let _ = fs::remove_dir_all(&lru_root);
    let _ = fs::remove_dir_all(&free_root);
}

#[test]
fn entries_iteration_exposes_hash_metadata_and_sequence() {
    let root = temp_root("entries");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let first = store.upload(b"first", meta("one"), Some("first".to_owned())).expect("upload lands");
    let second = store.upload(b"second", meta("two"), None).expect("upload lands");

    let mut rows: Vec<_> = store
        .entries()
        .map(|entry| (entry.hash.to_owned(), entry.metadata.label.clone(), entry.uploaded_seq))
        .collect();
    rows.sort_by_key(|(_, _, seq)| *seq);
    assert_eq!(rows.len(), 2);
    // A fresh store assigns sequence 1 to its first ingest (`next_seq`
    // restores to `max_uploaded_seq + 1`, and an empty store's max is 0).
    assert_eq!(rows[0], (first.clone(), "one".to_owned(), 1), "the first ingest carries sequence one");
    assert_eq!(rows[1], (second, "two".to_owned(), 2), "the second ingest carries the next sequence");
    assert_eq!(store.name_for(&first).as_deref(), Some("first"));
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: the on-disk sidecar flattens the metadata's own fields at the
/// object's top level next to `uploaded_seq` — no wrapper key. The hub's
/// `{ kind, manifest, uploaded_seq }` layout (its bit-for-bit regression
/// gate lives in `aether-fleet`) depends on this shape; if the
/// flatten were dropped the sidecar would gain a `metadata` wrapper and
/// this decode would read the wrong keys.
#[test]
fn sidecar_flattens_metadata_beside_the_sequence() {
    let root = temp_root("sidecar-shape");
    let hash = {
        let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
        store.upload(b"shape", meta("flat"), None).expect("upload lands")
    };
    let sidecar_path = root.join("entries").join(format!("{hash}.manifest"));
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(&sidecar_path).expect("read sidecar")).expect("decode sidecar JSON");
    let object = value.as_object().expect("sidecar is a JSON object");
    assert_eq!(object.get("label").and_then(serde_json::Value::as_str), Some("flat"), "metadata sits at top level");
    assert!(object.contains_key("uploaded_seq"), "the ingest sequence sits beside the metadata");
    assert!(!object.contains_key("metadata"), "the metadata is flattened, not wrapped");
    let _ = fs::remove_dir_all(&root);
}

/// A sidecar missing `uploaded_seq` (a legacy write) restores at sequence
/// zero — the `#[serde(default)]` on the flattened record's sequence field
/// works alongside the flatten.
#[test]
fn legacy_sidecar_without_sequence_restores_at_zero() {
    let root = temp_root("legacy-seq");
    let hash = {
        let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
        store.upload(b"legacy", meta("old"), None).expect("upload lands")
    };
    let sidecar_path = root.join("entries").join(format!("{hash}.manifest"));
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&sidecar_path).expect("read sidecar")).expect("decode sidecar JSON");
    value.as_object_mut().expect("sidecar is an object").remove("uploaded_seq");
    fs::write(&sidecar_path, serde_json::to_vec(&value).expect("re-encode legacy sidecar")).expect("write legacy");

    let store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let seq = store.entries().find(|entry| entry.hash == hash).map(|entry| entry.uploaded_seq);
    assert_eq!(seq, Some(0), "a legacy sidecar restores at sequence zero");
    let _ = fs::remove_dir_all(&root);
}

// Tripwire: a bytes-write failure must reach the caller as an error, never
// as a hash the store cannot resolve — pre-creating a directory at the
// entry's bytes path forces `atomic_write`'s rename to fail. A hash handed
// back here would resolve nowhere, turning a storage failure into a
// selector failure at whatever call site later spends it.
#[test]
fn failed_write_reports_an_error_and_leaves_no_dangling_name_pointer() {
    let root = temp_root("failed-write");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let bytes = b"never-lands";
    let hash = hash_hex(bytes);
    fs::create_dir_all(root.join("entries").join(&hash)).expect("pre-create a directory at the bytes path");

    let result = store.upload(bytes, meta("x"), Some("svc".to_owned()));

    assert!(result.is_err(), "a write that never landed reports the storage failure: {result:?}");
    assert!(!store.contains(&hash), "the failed write leaves the hash unindexed");
    assert_eq!(store.name_for(&hash), None, "no dangling name pointer at an unindexed hash");
    assert!(store.get(&Selector::Name("svc".to_owned())).is_none(), "the name resolves nowhere");
    let _ = fs::remove_dir_all(&root);
}

/// Open two handles over `root` in the order the shipping arrangement
/// does: `first` comes up (a chassis mounting a capability), then
/// `second` (a reactor opening its own handle) — so `first`'s index was
/// built before any of `second`'s writes existed.
fn two_handles(root: &Path) -> (ContentStore<Meta>, ContentStore<Meta>) {
    let first = ContentStore::open(root, EvictionPolicy::None).expect("open first handle");
    let second = ContentStore::open(root, EvictionPolicy::None).expect("open second handle");
    (first, second)
}

/// Tripwire: a handle resolves an entry a peer handle wrote to the same
/// root after it opened. `restore` runs once at open, so the index is a
/// snapshot; if `get` answered from it alone, the mounted bloomery
/// artifacts capability would reply `NotFound` for every record the
/// executor reactor's handle filed after boot — bytes that are on disk,
/// written by the same process moments earlier.
#[test]
fn a_peer_handles_upload_resolves_through_an_older_handle() {
    let root = temp_root("peer-get");
    let (mut first, mut second) = two_handles(&root);

    let hash = second.upload(b"filed-by-the-peer", meta("peer"), None).expect("upload lands");

    assert!(!first.contains(&hash), "the older handle's index has not seen the peer's write");
    let resolved =
        first.get(&Selector::Hash(hash.clone())).expect("the peer's entry resolves through the older handle");
    assert_eq!(resolved.hash, hash);
    assert_eq!(resolved.metadata, meta("peer"), "the peer's sidecar metadata comes back, not a placeholder");
    assert_eq!(fs::read(&resolved.path).expect("the stored bytes are readable"), b"filed-by-the-peer");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: an upload of bytes a peer handle already stored dedups onto
/// the peer's entry instead of rewriting it. Dedup decided against this
/// handle's index alone would take the `persist_new` branch and overwrite
/// the sidecar — in bloomery that sidecar is `ArtifactMeta::parents`, so
/// a peer's recorded derivation edge would vanish with nothing logged
/// (the dropped-parents warning cannot fire on a write that looks new).
#[test]
fn upload_dedups_onto_a_peer_written_entry_and_keeps_its_metadata() {
    let root = temp_root("peer-upload");
    let (mut first, mut second) = two_handles(&root);

    let peer_hash = second.upload(b"shared-bytes", meta("the-peers-parents"), None).expect("upload lands");
    let own_hash = first.upload(b"shared-bytes", meta("a-later-callers-parents"), None).expect("upload lands");

    assert_eq!(own_hash, peer_hash, "identical bytes address the same entry across handles");
    let resolved = first.get(&Selector::Hash(peer_hash.clone())).expect("the entry resolves");
    assert_eq!(resolved.metadata, meta("the-peers-parents"), "the peer's sidecar survived the second handle's upload");

    // The sidecar on disk is the record both handles and any restart read.
    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("entries").join(format!("{peer_hash}.manifest"))).expect("read"))
            .expect("decode sidecar JSON");
    assert_eq!(
        sidecar.get("label").and_then(serde_json::Value::as_str),
        Some("the-peers-parents"),
        "the on-disk sidecar was not rewritten under the later caller's metadata",
    );

    // And the peer's own view is unchanged — nothing clobbered it either.
    assert_eq!(
        second.get(&Selector::Hash(peer_hash)).expect("the peer still resolves its entry").metadata,
        meta("the-peers-parents"),
    );
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: a name a peer handle pointed after this handle opened
/// resolves. `names.json` is rewritten atomically by whichever handle
/// uploads, so the in-memory name map is as stale as the entry index —
/// a name miss re-reads the file before it becomes a `None`.
#[test]
fn a_peer_pointed_name_resolves_through_an_older_handle() {
    let root = temp_root("peer-name");
    let (mut first, mut second) = two_handles(&root);

    let hash = second.upload(b"named-by-the-peer", meta("peer"), Some("svc".to_owned())).expect("upload lands");

    let resolved = first.get(&Selector::Name("svc".to_owned())).expect("the peer's name resolves");
    assert_eq!(resolved.hash, hash);
    assert_eq!(resolved.metadata, meta("peer"));
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: the two selectors agree on the resolved entry's name. The
/// hash path adopts on a miss and the name path re-reads `names.json`, so
/// an adopt that took the entry without its name would make
/// `Resolved::name` depend on which selector happened to reach it —
/// `None` by hash, `Some` by name, for one entry at one instant. That is
/// a wrong answer rather than a stale one, and it reaches a caller:
/// `aether-fleet` hands `Resolved::name` straight out as a resolved
/// component's name.
#[test]
fn both_selectors_resolve_a_peer_pointed_name_the_same_way() {
    let root = temp_root("peer-name-agreement");
    let (mut first, mut second) = two_handles(&root);

    let hash = second.upload(b"named-by-the-peer", meta("peer"), Some("svc".to_owned())).expect("upload lands");

    // The hash path reaches the entry first, so it is the one that adopts.
    let by_hash = first.get(&Selector::Hash(hash.clone())).expect("the entry resolves by hash");
    assert_eq!(by_hash.name.as_deref(), Some("svc"), "an entry adopted by hash carries the name the root records");
    assert_eq!(first.name_for(&hash).as_deref(), Some("svc"));

    let by_name = first.get(&Selector::Name("svc".to_owned())).expect("the entry resolves by name");
    assert_eq!(by_name.name, by_hash.name, "both selectors agree on the resolved name");
    assert_eq!(by_name.hash, by_hash.hash);
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: `refresh` is the enumeration-side counterpart of the
/// miss-path adopt. `entries` / `entry_count` answer from the index with
/// no disk read of their own, so a projection rebuild over a shared root
/// (bloomery's `rebuild_study_index`) enumerates only its own handle's
/// writes until it refreshes.
#[test]
fn refresh_adopts_peer_written_entries_into_enumeration() {
    let root = temp_root("peer-refresh");
    let (mut first, mut second) = two_handles(&root);

    let own = first.upload(b"mine", meta("mine"), None).expect("upload lands");
    let peer = second.upload(b"theirs", meta("theirs"), Some("svc".to_owned())).expect("upload lands");
    assert_eq!(first.entry_count(), 1, "enumeration sees only this handle's own write before a refresh");

    first.refresh();

    let mut rows: Vec<(String, String)> =
        first.entries().map(|entry| (entry.hash.to_owned(), entry.metadata.label.clone())).collect();
    rows.sort();
    let mut expected = vec![(own, "mine".to_owned()), (peer.clone(), "theirs".to_owned())];
    expected.sort();
    assert_eq!(rows, expected, "refresh adopts the peer's entry into enumeration");
    assert_eq!(first.entry_count(), 2);
    assert_eq!(first.name_for(&peer).as_deref(), Some("svc"), "the peer's name comes across too");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: a pin rides the entry's own sidecar, so it is a property of
/// the root rather than of the handle that set it — an entry a peer
/// pinned must arrive protected when this handle indexes it. Adopting it
/// unpinned would let a second handle evict an artifact the root records
/// as pinned, and it is the same read path a restart takes, so the pin
/// the hub set before its last shutdown would come back inert.
#[test]
fn refresh_adopts_a_peer_pinned_entry_as_protected() {
    let root = temp_root("peer-refresh-pin");
    // Ten-byte payloads against a 15-byte budget: the trigger upload must
    // evict every unprotected entry to get back under it.
    let mut first: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(15)).expect("open store");
    let mut second: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open peer handle");

    let pinned = second.upload(b"pinned-aaa", meta("a"), None).expect("upload lands");
    assert!(second.pin(&pinned), "pin targets a stored entry");
    first.upload(b"plain-bbbb", meta("b"), None).expect("upload lands");

    first.refresh();
    first.upload(b"trigger-cc", meta("c"), None).expect("upload lands");

    assert_eq!(first.entry_count(), 1, "every unpinned entry was reclaimed to hold the budget");
    assert!(first.contains(&pinned), "the peer's pin came across with the entry and protected it from eviction");
    let _ = fs::remove_dir_all(&root);
}

/// Occupy the sidecar path with a directory so `atomic_write`'s rename
/// fails. Permissions are not used: root bypasses them. Only the
/// test-owned path under `root` is touched.
fn sidecar_path(root: &Path, hash: &str) -> PathBuf {
    root.join("entries").join(format!("{hash}.manifest"))
}

fn occupy_sidecar_as_dir(root: &Path, hash: &str) {
    let path = sidecar_path(root, hash);
    if path.is_file() {
        fs::remove_file(&path).expect("remove the real sidecar so the path can become a directory");
    }
    fs::create_dir_all(&path).expect("sidecar target is a directory");
}

fn restore_writable_sidecar_path(root: &Path, hash: &str) {
    let path = sidecar_path(root, hash);
    if path.is_dir() {
        fs::remove_dir_all(&path).expect("remove the sabotaged sidecar directory");
    }
}

/// Tripwire: an unnamed `pin: true` upload must record protection in its
/// first sidecar, before eviction. Budget 0 (and a body larger than that
/// budget) would otherwise reclaim the entry on the same call.
#[test]
fn unnamed_pin_true_survives_budget_zero() {
    let root = temp_root("pin-budget0");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(0)).expect("open store");
    let pinned = store.upload_with_pin(b"pinned-body-larger-than-zero", meta("p"), None, true).expect("pinned upload");
    assert!(store.contains(&pinned), "an unnamed pin:true upload survives a zero budget");

    let plain = store.upload_with_pin(b"plain-body-larger-than-zero", meta("u"), None, false).expect("unpinned upload");
    assert!(!store.contains(&plain), "an unnamed unpinned upload is evictable under a zero budget");
    assert!(store.contains(&pinned), "the pinned sibling is still protected after the unpinned upload");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: `pin: true` on a dedup must persist before *that* upload's
/// eviction. Seed without pressure, reopen under budget 0 (open itself
/// does not evict), then dedup-pin while already over budget.
#[test]
fn dedup_pin_true_upgrades_before_eviction() {
    let root = temp_root("dedup-pin");
    let bytes = b"shared-aa";
    let hash = {
        let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
        store.upload_with_pin(bytes, meta("a"), None, false).expect("seed without pressure")
    };

    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(0)).expect("reopen budget 0");
    assert!(
        store.contains(&hash),
        "open does not evict; the unpinned seed is still indexed under a zero budget"
    );
    let again = store
        .upload_with_pin(bytes, meta("a"), None, true)
        .expect("dedup pin:true faces same-call zero-budget pressure");
    assert_eq!(again, hash);
    assert!(store.contains(&hash), "pin must persist before this upload's eviction");
    drop(store);

    let mut reopened: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(0)).expect("reopen");
    assert!(reopened.contains(&hash), "the durable pin is still indexed after reopen under a zero budget");
    reopened.upload_with_pin(b"trigger-bbbbbbbb", meta("t"), None, false).expect("trigger");
    assert!(reopened.contains(&hash), "the durable pin survives another reopen and a later eviction");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: `pin: false` is not unpin. Re-uploading pinned bytes without
/// asking for a pin must leave the durable flag in place across reopen.
#[test]
fn pin_false_reupload_preserves_an_existing_pin() {
    let root = temp_root("pin-false-keep");
    let bytes = b"keep-me-aa";
    let hash = {
        let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
        let hash = store.upload_with_pin(bytes, meta("a"), None, true).expect("pinned upload");
        let again = store.upload_with_pin(bytes, meta("a"), None, false).expect("pin:false reupload");
        assert_eq!(again, hash);
        hash
    };
    let mut reopened: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(0)).expect("reopen");
    assert!(reopened.contains(&hash), "pin:false must not clear an existing pin across reopen");
    reopened.upload_with_pin(b"trigger-bbbbbbbb", meta("t"), None, false).expect("trigger");
    assert!(reopened.contains(&hash), "the preserved pin still protects after reopen eviction");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn try_set_pinned_unknown_hash_is_false() {
    let root = temp_root("pin-unknown");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let result = store.try_set_pinned("not-a-stored-hash", true).expect("unknown hash is not an IO error");
    assert!(!result, "an unknown hash returns Ok(false)");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: operator pin/unpin must survive close/reopen because the
/// sidecar is the authority, not the handle that set the flag.
#[test]
fn try_set_pinned_survives_reopen_for_pin_and_unpin() {
    let root = temp_root("try-pin-reopen");
    let (h_pinned, h_released) = {
        let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
        let h_pinned = store.upload_with_pin(b"pinned-aaaa", meta("a"), None, false).expect("upload");
        let h_released = store.upload_with_pin(b"released-aa", meta("b"), None, false).expect("upload");
        assert!(store.try_set_pinned(&h_pinned, true).expect("pin persists"));
        assert!(store.try_set_pinned(&h_released, true).expect("pin before unpin"));
        assert!(store.try_set_pinned(&h_released, false).expect("unpin persists"));
        (h_pinned, h_released)
    };

    let mut reopened: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(20)).expect("reopen");
    // The released entry is unnamed and unpinned, so it is eligible. A
    // trigger upload over a budget that cannot hold every entry evicts
    // the oldest eligible candidate. `get` bumps recency; it does not
    // make an entry LRU.
    reopened.upload_with_pin(b"trigger-ccc", meta("t"), None, false).expect("trigger");
    assert!(reopened.contains(&h_pinned), "a persisted pin still protects after reopen");
    assert!(!reopened.contains(&h_released), "a persisted unpin returns the entry to the candidates");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: legacy `set_pinned` may leave memory true after a failed
/// sidecar write. Durable `try_set_pinned(true)` must still persist
/// (and fail while the path is blocked) rather than shortcut on equal
/// in-memory state.
#[test]
fn try_set_pinned_does_not_shortcut_equal_in_memory_state() {
    let root = temp_root("pin-equal-state");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let hash = store.upload_with_pin(b"plain-aaaa", meta("a"), None, false).expect("upload");
    occupy_sidecar_as_dir(store.root(), &hash);

    assert!(store.set_pinned(&hash, true), "legacy set_pinned returns true after mutating memory");
    store
        .try_set_pinned(&hash, true)
        .expect_err("durable pin must still persist even when memory is already true");

    restore_writable_sidecar_path(store.root(), &hash);
    assert!(
        store.try_set_pinned(&hash, true).expect("repair allows persist despite memory already true"),
        "equal in-memory state still writes the sidecar"
    );
    drop(store);

    let mut reopened: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(0)).expect("reopen");
    assert!(reopened.contains(&hash), "the equal-state persist restored the entry");
    reopened.upload_with_pin(b"trigger-bbbbbbbb", meta("t"), None, false).expect("trigger");
    assert!(reopened.contains(&hash), "reopen confirms the sidecar actually recorded the pin");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: a pin persistence failure is observable and must not claim
/// success or change in-memory protection. Occupying the sidecar path
/// with a directory forces `atomic_write` to fail without relying on
/// permissions root can bypass.
#[test]
fn try_set_pinned_failure_leaves_in_memory_state_unchanged() {
    let root = temp_root("pin-fail");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(20)).expect("open store");
    let hash = store.upload_with_pin(b"plain-aaaa", meta("a"), None, false).expect("upload");
    occupy_sidecar_as_dir(store.root(), &hash);

    let err = store.try_set_pinned(&hash, true).expect_err("a directory sidecar target is a persistence failure");
    assert!(err.raw_os_error().is_some() || err.kind() != io::ErrorKind::Other, "the failure is an IO category: {err}");

    store.upload_with_pin(b"trigger-bbbbbbbb", meta("t"), None, false).expect("trigger");
    assert!(!store.contains(&hash), "a failed pin must leave the entry evictable");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: a failed unpin must keep prior protection, including in
/// memory, so a persistence miss cannot silently expose a pinned fixture.
#[test]
fn try_set_unpin_failure_keeps_prior_protection() {
    let root = temp_root("unpin-fail");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::LruBudget(20)).expect("open store");
    let hash = store.upload_with_pin(b"pinned-aaaa", meta("a"), None, true).expect("pinned upload");
    occupy_sidecar_as_dir(store.root(), &hash);

    store.try_set_pinned(&hash, false).expect_err("a directory sidecar target is a persistence failure");
    store.upload_with_pin(b"trigger-bbbbbbbb", meta("t"), None, false).expect("trigger");
    assert!(store.contains(&hash), "a failed unpin must keep the in-memory pin");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: a new `pin: true` upload whose sidecar cannot persist must
/// not return a hash or point a name at content the store cannot pin.
#[test]
fn pin_true_new_upload_write_failure_does_not_return_a_hash() {
    let root = temp_root("pin-new-fail");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let bytes = b"never-pinned";
    let hash = hash_hex(bytes);
    occupy_sidecar_as_dir(store.root(), &hash);

    let result = store.upload_with_pin(bytes, meta("x"), Some("svc".to_owned()), true);
    assert!(result.is_err(), "a pin:true sidecar failure must not succeed: {result:?}");
    assert!(!store.contains(&hash), "the failed pin upload leaves the hash unindexed");
    assert_eq!(store.name_for(&hash), None, "no name is pointed at an unpinned failed upload");
    let _ = fs::remove_dir_all(&root);
}

/// Tripwire: a dedup `pin: true` that cannot persist must not return a
/// successful pinned hash or repoint a name.
#[test]
fn dedup_pin_true_failure_does_not_repoint_a_name() {
    let root = temp_root("dedup-pin-fail");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let hash = store.upload_with_pin(b"shared-bytes", meta("a"), None, false).expect("first upload");
    occupy_sidecar_as_dir(store.root(), &hash);

    let result = store.upload_with_pin(b"shared-bytes", meta("a"), Some("svc".to_owned()), true);
    assert!(result.is_err(), "a failed dedup pin must not succeed: {result:?}");
    assert_eq!(store.name_for(&hash), None, "a failed dedup pin must not repoint a name");
    let _ = fs::remove_dir_all(&root);
}
