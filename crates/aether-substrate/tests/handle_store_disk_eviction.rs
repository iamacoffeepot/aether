#![allow(clippy::disallowed_methods)] // integration test — spawns threads; no settlement contract
//! Issue #986: disk eviction by created_at-LRU (ADR-0049 §5).
//!
//! When the on-disk byte ledger exceeds the budget, the eviction tick
//! drops refcount-0 + unpinned entries oldest-first via a two-phase
//! delete. Pinned + in-use entries stay. A crash between phases leaves
//! an orphan the boot scrub catches. Tests trigger the eviction pass
//! synchronously via `run_disk_eviction` for determinism.

#![allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic-on-failure is the assertion"
)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_data::{HandleId, KindId};
use aether_substrate::handle_store::meta::TransformOrigin;
use aether_substrate::handle_store::{HandleStore, PersistConfig, entry_paths};

static NONCE: AtomicU64 = AtomicU64::new(0);

fn scratch_root(tag: &str) -> PathBuf {
    let pid = process::id();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("aether-handle-evict-{tag}-{pid}-{millis}-{n}"));
    fs::create_dir_all(&path).expect("test setup: scratch dir creates");
    path
}

fn cleanup(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn cfg_with_budget(root: &Path, budget: u64) -> PersistConfig {
    PersistConfig {
        root: root.join("v1"),
        disk_budget_bytes: budget,
        eviction_tick_secs: 60,
    }
}

/// Sum of `.bin` file sizes under entries/ — the actual on-disk usage
/// (the `du`-equivalent the plan references, restricted to payloads).
fn actual_bin_bytes(cfg: &PersistConfig) -> u64 {
    let mut total = 0u64;
    let entries = cfg.root.join("entries");
    let Ok(shards) = fs::read_dir(&entries) else {
        return 0;
    };
    for shard in shards.flatten() {
        if let Ok(files) = fs::read_dir(shard.path()) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("bin") {
                    total += f.metadata().map_or(0, |m| m.len());
                }
            }
        }
    }
    total
}

#[test]
fn eviction_skips_pinned_handles() {
    let root = scratch_root("skip-pinned");
    // 10 entries × ~1MB = ~10MB; budget 1MB forces eviction.
    let cfg = cfg_with_budget(&root, 1024 * 1024);
    let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
    for i in 0..10u64 {
        let id = HandleId(i + 1);
        store
            .put_persistent(id, KindId(7), vec![0u8; 1024 * 1024], None)
            .unwrap();
        if i < 5 {
            store.pin(id);
        }
    }
    store.run_disk_eviction();

    for i in 0..10u64 {
        let id = HandleId(i + 1);
        let (bin, _) = entry_paths(&cfg.root, id);
        if i < 5 {
            assert!(bin.exists(), "pinned id {i} survives");
        } else {
            assert!(!bin.exists(), "unpinned id {i} evicted");
        }
    }
    cleanup(&root);
}

#[test]
fn eviction_orders_by_created_at() {
    let root = scratch_root("order");
    let cfg = cfg_with_budget(&root, 7 * 1024 * 1024);
    let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
    // Write 10 × 1MB with a tiny gap so created_at is monotonic.
    for i in 0..10u64 {
        store
            .put_persistent(HandleId(i + 1), KindId(7), vec![0u8; 1024 * 1024], None)
            .unwrap();
        thread::sleep(Duration::from_millis(2));
    }
    // Budget 7MB on a 10MB store → evict the 3 oldest.
    store.run_disk_eviction();

    for i in 0..3u64 {
        let (bin, _) = entry_paths(&cfg.root, HandleId(i + 1));
        assert!(!bin.exists(), "oldest id {i} evicted");
    }
    for i in 3..10u64 {
        let (bin, _) = entry_paths(&cfg.root, HandleId(i + 1));
        assert!(bin.exists(), "newer id {i} survives");
    }
    cleanup(&root);
}

#[test]
fn eviction_skips_in_use_handles() {
    let root = scratch_root("in-use");
    let cfg = cfg_with_budget(&root, 0);
    let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
    let id = HandleId(1);
    store
        .put_persistent(id, KindId(7), vec![0u8; 1024], None)
        .unwrap();
    // refcount 1 protects it.
    store.inc_ref(id);
    store.run_disk_eviction();

    let (bin, _) = entry_paths(&cfg.root, id);
    assert!(bin.exists(), "refcounted entry stays even at budget 0");
    cleanup(&root);
}

#[test]
fn eviction_tick_yields_when_budget_clear() {
    let root = scratch_root("under-budget");
    let cfg = cfg_with_budget(&root, 1024 * 1024 * 1024);
    let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
    for i in 0..5u64 {
        store
            .put_persistent(HandleId(i + 1), KindId(7), vec![0u8; 1024], None)
            .unwrap();
    }
    store.run_disk_eviction();
    // All 5 survive — budget far from exceeded.
    for i in 0..5u64 {
        let (bin, _) = entry_paths(&cfg.root, HandleId(i + 1));
        assert!(bin.exists(), "id {i} not evicted under budget");
    }
    cleanup(&root);
}

#[test]
fn eviction_two_phase_handles_crash_between_phases() {
    // Simulate a crash mid-eviction: delete just the .bin, leaving an
    // orphan .meta. The boot scrub on the next start deletes the .meta.
    let root = scratch_root("crash-between");
    let cfg = cfg_with_budget(&root, u64::MAX);
    let id = HandleId(0x77);
    {
        let store = HandleStore::with_persist(64 * 1024, Some(cfg.clone()));
        store
            .put_persistent(id, KindId(1), b"payload".to_vec(), None)
            .unwrap();
    }
    let (bin, meta) = entry_paths(&cfg.root, id);
    fs::remove_file(&bin).unwrap();
    assert!(meta.exists());
    // Restart → boot scrub removes the orphan meta.
    let restored = HandleStore::with_persist(64 * 1024, Some(cfg));
    assert!(!meta.exists(), "boot scrub removed orphan meta");
    assert_eq!(restored.disk_index_len(), 0);
    cleanup(&root);
}

#[test]
fn eviction_recovers_from_orphan_bin() {
    let root = scratch_root("orphan-bin");
    let cfg = cfg_with_budget(&root, u64::MAX);
    let id = HandleId(0x88);
    let (bin, _) = entry_paths(&cfg.root, id);
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::write(&bin, b"orphan-bytes").unwrap();
    // Restart → boot scrub removes the orphan bin.
    let _restored = HandleStore::with_persist(64 * 1024, Some(cfg));
    assert!(!bin.exists(), "boot scrub removed orphan bin");
    cleanup(&root);
}

/// Spawns 10 writer threads racing `put_persistent` and joins them.
#[test]
fn eviction_ledger_stays_consistent_under_concurrent_writes() {
    let root = scratch_root("ledger");
    let cfg = cfg_with_budget(&root, u64::MAX);
    let store = Arc::new(HandleStore::with_persist(
        256 * 1024 * 1024,
        Some(cfg.clone()),
    ));
    // 10 threads × 100 distinct entries × 256 bytes.
    let handles: Vec<_> = (0..10u64)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                for i in 0..100u64 {
                    let id = HandleId(t * 1000 + i + 1);
                    let origin = TransformOrigin {
                        component_mailbox: t,
                        transform_index: 0,
                        input_handle_ids: vec![],
                    };
                    store
                        .put_persistent(id, KindId(7), vec![0u8; 256], Some(origin))
                        .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // Distinct ids → ledger equals actual on-disk bin bytes.
    assert_eq!(store.disk_bytes(), actual_bin_bytes(&cfg));
    assert_eq!(store.disk_bytes(), 1000 * 256);
    cleanup(&root);
}

#[test]
fn eviction_after_in_memory_drop_still_finds_disk_entry() {
    // A handle written persistently, then dropped from the in-memory
    // store via budget pressure, remains on disk + indexed and is
    // eviction-eligible.
    let root = scratch_root("disk-only");
    let cfg = cfg_with_budget(&root, 100);
    let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
    let id = HandleId(0x99);
    store
        .put_persistent(id, KindId(7), vec![0u8; 4096], None)
        .unwrap();
    // It's both in memory and indexed on disk.
    assert!(store.contains(id));
    // Eviction over the 100-byte budget removes the unpinned, refcount-0
    // disk entry.
    store.run_disk_eviction();
    let (bin, _) = entry_paths(&cfg.root, id);
    assert!(!bin.exists(), "disk entry evicted under budget pressure");
    cleanup(&root);
}
