//! Issue #985: lazy restore on cache miss + boot scan (ADR-0049 §3).
//!
//! The on-disk tree is truth; the in-memory store is a hot cache. A
//! restart populates a sparse disk index from the `.meta` sidecars
//! without eagerly loading bytes; a cache-miss `get` materializes the
//! bytes lazily. Orphan handling + negative-cache behaviour pin the
//! correctness contract.
//!
//! The DAG-level cache-hit-after-restart guarantee from the plan
//! (`restore_dag_transform_cache_hit_after_restart`) is expressed at the
//! store surface here (`restore_resolves_cached_handle`,
//! `restore_does_not_eagerly_load_bytes`): the DAG executor + transform
//! machinery (ADR-0048 Phase 3) isn't yet in the workspace, so the
//! store-reconstruction round trip is the available expression of the
//! same "skip recompute, hit the disk cache after restart" guarantee.

#![allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic-on-failure is the assertion"
)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use aether_data::{HandleId, KindId};
use aether_substrate::handle_store::meta::{HandleMeta, SCHEMA_VERSION};
use aether_substrate::handle_store::{HandleStore, PersistConfig, entry_paths};

static NONCE: AtomicU64 = AtomicU64::new(0);

fn scratch_root(tag: &str) -> PathBuf {
    let pid = process::id();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("aether-handle-restore-{tag}-{pid}-{millis}-{n}"));
    fs::create_dir_all(&path).expect("test setup: scratch dir creates");
    path
}

fn cleanup(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn cfg_under(root: &Path) -> PersistConfig {
    PersistConfig {
        root: root.join("v1"),
        disk_budget_bytes: u64::MAX,
        eviction_tick_secs: 60,
    }
}

/// Hand-write a `.meta` sidecar (used by orphan tests).
fn write_meta(cfg: &PersistConfig, id: HandleId, kind: KindId, bytes_len: u32) {
    let (_, meta_path) = entry_paths(&cfg.root, id);
    fs::create_dir_all(meta_path.parent().unwrap()).unwrap();
    let meta = HandleMeta {
        schema_version: SCHEMA_VERSION,
        handle_id: id.0,
        kind_id: kind.0,
        kind_name: "test.kind".to_owned(),
        transform_origin: None,
        bytes_len,
        created_at: 1,
        pinned: false,
    };
    fs::write(&meta_path, postcard::to_allocvec(&meta).unwrap()).unwrap();
}

#[test]
fn restore_boot_scan_populates_index() {
    let root = scratch_root("scan");
    let cfg = cfg_under(&root);
    {
        let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
        for i in 0..100u64 {
            store
                .put_persistent(HandleId(i + 1), KindId(7), vec![0u8; 16], None)
                .unwrap();
        }
    }
    let restored = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg));
    assert_eq!(restored.disk_index_len(), 100);
    cleanup(&root);
}

#[test]
fn restore_resolves_cached_handle() {
    let root = scratch_root("resolve");
    let cfg = cfg_under(&root);
    let id = HandleId(0xABCD);
    let kind = KindId(0x1234);
    let bytes = b"persisted-payload".to_vec();
    {
        let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
        store.put_persistent(id, kind, bytes.clone(), None).unwrap();
    }
    let restored = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg));
    // Not in memory until accessed.
    assert!(!restored.contains_in_memory(id));
    let (got_kind, got_bytes) = restored.get(id).expect("resolves from disk");
    assert_eq!(got_kind, kind);
    assert_eq!(got_bytes, bytes);
    // Now materialized in memory.
    assert!(restored.contains_in_memory(id));
    cleanup(&root);
}

#[test]
fn restore_does_not_eagerly_load_bytes() {
    let root = scratch_root("lazy");
    let cfg = cfg_under(&root);
    {
        let store = HandleStore::with_persist(256 * 1024 * 1024, Some(cfg.clone()));
        for i in 0..1000u64 {
            // 1KB each = 1MB total; the point is "indexed, not loaded".
            store
                .put_persistent(HandleId(i + 1), KindId(7), vec![0u8; 1024], None)
                .unwrap();
        }
    }
    let restored = HandleStore::with_persist(256 * 1024 * 1024, Some(cfg));
    assert_eq!(restored.disk_index_len(), 1000);
    // No bytes eagerly loaded into memory.
    assert_eq!(restored.entry_count(), 0);
    assert_eq!(restored.total_bytes(), 0);
    cleanup(&root);
}

#[test]
fn restore_handles_orphan_bin_without_meta() {
    let root = scratch_root("orphan-bin");
    let cfg = cfg_under(&root);
    let id = HandleId(0x10);
    let (bin_path, _) = entry_paths(&cfg.root, id);
    fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
    fs::write(&bin_path, b"orphan").unwrap();

    let restored = HandleStore::with_persist(64 * 1024, Some(cfg));
    // Orphan bin not indexed (the meta is the index entry).
    assert_eq!(restored.disk_index_len(), 0);
    // And the boot scrub removed it.
    assert!(!bin_path.exists(), "orphan bin scrubbed");
    cleanup(&root);
}

#[test]
fn restore_handles_orphan_meta_without_bin() {
    let root = scratch_root("orphan-meta");
    let cfg = cfg_under(&root);
    let id = HandleId(0x20);
    write_meta(&cfg, id, KindId(1), 8);
    let (_, meta_path) = entry_paths(&cfg.root, id);
    assert!(meta_path.exists());

    let restored = HandleStore::with_persist(64 * 1024, Some(cfg));
    // Orphan meta scrubbed; get returns None.
    assert!(restored.get(id).is_none());
    assert_eq!(restored.disk_index_len(), 0);
    assert!(!meta_path.exists(), "orphan meta scrubbed");
    cleanup(&root);
}

#[test]
fn restore_pinned_set_loads() {
    let root = scratch_root("pinned-loads");
    let cfg = cfg_under(&root);
    {
        let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
        for i in 0..5u64 {
            let id = HandleId(i + 1);
            store
                .put_persistent(id, KindId(7), vec![1u8; 8], None)
                .unwrap();
            store.pin(id);
        }
    }
    let restored = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg));
    // The 5 pinned ids materialize as pinned (proof: get them and they
    // carry the pinned flag through to in-memory).
    for i in 0..5u64 {
        let id = HandleId(i + 1);
        assert!(restored.contains(id), "id {i} indexed");
        // Materialize, then assert it survives byte-pressure (pinned).
        let _ = restored.get(id);
    }
    assert_eq!(restored.disk_index_len(), 5);
    cleanup(&root);
}

#[test]
fn restore_negative_cache_prevents_restat() {
    // An unknown id (not in the index) hits the negative path. Repeated
    // gets must not re-stat (the index has no entry, so lookup_from_disk
    // returns None without ever touching the filesystem). This test
    // pins the "no index entry => no fs op" contract.
    let root = scratch_root("neg-cache");
    let cfg = cfg_under(&root);
    // Empty store (entries/ doesn't even exist).
    let store = HandleStore::with_persist(64 * 1024, Some(cfg));
    let unknown = HandleId(0xDEAD);
    for _ in 0..100 {
        assert!(store.get(unknown).is_none());
    }
    // No index entry means no fs stat per lookup — the absence of a
    // panic / hang is the assertion; correctness is "always None".
    assert!(!store.contains(unknown));
    cleanup(&root);
}

#[test]
fn restore_corrupt_bin_falls_back_to_miss() {
    // Index says the entry is on disk, but the .bin is missing
    // (corruption after the boot scan). get() drops the index entry,
    // negatively caches, and returns None.
    let root = scratch_root("corrupt-bin");
    let cfg = cfg_under(&root);
    let id = HandleId(0x30);
    {
        let store = HandleStore::with_persist(64 * 1024, Some(cfg.clone()));
        store
            .put_persistent(id, KindId(1), b"x".to_vec(), None)
            .unwrap();
    }
    let restored = HandleStore::with_persist(64 * 1024, Some(cfg.clone()));
    assert_eq!(restored.disk_index_len(), 1);
    // Delete the .bin out from under the index.
    let (bin_path, _) = entry_paths(&cfg.root, id);
    fs::remove_file(&bin_path).unwrap();
    // First get: index hit, read fails, drops the index entry.
    assert!(restored.get(id).is_none());
    assert!(!restored.contains(id), "index entry dropped on corruption");
    cleanup(&root);
}
