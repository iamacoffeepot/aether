//! Domain-clean core tests: content addressing, dedup, persistence,
//! pinning, and the eviction-policy parameter. These exercise the storage
//! primitive over a trivial metadata type `M` — the hub's binary /
//! component projections are tested against the real manifests in
//! `aether-engine`.

use super::{ContentStore, EvictionPolicy, Selector, hash_hex, now_nanos};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
    let h1 = store.upload(b"the-bytes", meta("a"), None);
    let h2 = store.upload(b"the-bytes", meta("a"), None);
    assert_eq!(h1, h2, "identical bytes dedup to the same content hash");
    assert_eq!(store.entry_count(), 1, "dedup stores one entry");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn name_repoints_to_the_latest_uploaded_hash() {
    let root = temp_root("repoint");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let h_old = store.upload(b"v1", meta("a"), Some("svc".to_owned()));
    let h_new = store.upload(b"v2", meta("a"), Some("svc".to_owned()));
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
        store.upload(b"persisted-bytes", meta("keep"), Some("svc".to_owned()))
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
    let h_plain = store.upload(b"plain-aaaa", meta("a"), None);
    let h_named = store.upload(b"named-bbbb", meta("a"), Some("keep".to_owned()));
    let h_pinned = store.upload(b"pinned-ccc", meta("a"), None);
    assert!(store.pin(&h_pinned), "pin targets a stored entry");
    let _ = store.upload(b"trigger-ddd", meta("a"), None);

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
        let plain = store.upload(b"plain-aaaa", meta("a"), None);
        store.upload(b"named-bbbb", meta("a"), Some("keep".to_owned()));
        store.upload(b"pinned-ccc", meta("a"), None);
        // Trigger an over-budget upload.
        store.upload(b"trigger-ddd", meta("a"), None);
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
    let first = store.upload(b"first", meta("one"), Some("first".to_owned()));
    let second = store.upload(b"second", meta("two"), None);

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
/// gate lives in `aether-engine`) depends on this shape; if the
/// flatten were dropped the sidecar would gain a `metadata` wrapper and
/// this decode would read the wrong keys.
#[test]
fn sidecar_flattens_metadata_beside_the_sequence() {
    let root = temp_root("sidecar-shape");
    let hash = {
        let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
        store.upload(b"shape", meta("flat"), None)
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
        store.upload(b"legacy", meta("old"), None)
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

// Tripwire: a bytes-write failure must never leave a name pointer at an
// unindexed hash — pre-creating a directory at the entry's bytes path forces
// `atomic_write`'s rename to fail, so this fails on the unguarded code and
// passes once the name insert is gated on `self.entries.contains_key`.
#[test]
fn failed_write_leaves_no_dangling_name_pointer() {
    let root = temp_root("failed-write");
    let mut store: ContentStore<Meta> = ContentStore::open(&root, EvictionPolicy::None).expect("open store");
    let bytes = b"never-lands";
    let hash = hash_hex(bytes);
    fs::create_dir_all(root.join("entries").join(&hash)).expect("pre-create a directory at the bytes path");

    let returned = store.upload(bytes, meta("x"), Some("svc".to_owned()));

    assert_eq!(returned, hash, "upload still returns the content hash");
    assert!(!store.contains(&hash), "the failed write leaves the hash unindexed");
    assert_eq!(store.name_for(&hash), None, "no dangling name pointer at an unindexed hash");
    let _ = fs::remove_dir_all(&root);
}
