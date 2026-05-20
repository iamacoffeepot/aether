//! Issue #987: on-disk store lockfile (ADR-0049 §7).
//!
//! `lock.pid` records the owning substrate's PID; boot reads it,
//! reclaims a stale lock (dead PID) with a warning, and aborts on a
//! live conflict. A `LockGuard` deletes the file on graceful shutdown.

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

use aether_substrate::handle_store::{HandleStore, LockError, PersistConfig};

static NONCE: AtomicU64 = AtomicU64::new(0);

fn scratch_root(tag: &str) -> PathBuf {
    let pid = process::id();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("aether-handle-lock-{tag}-{pid}-{millis}-{n}"));
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

#[test]
fn lockfile_acquired_on_boot() {
    let root = scratch_root("acquire");
    let cfg = cfg_under(&root);
    let store = HandleStore::with_persist(64 * 1024, Some(cfg.clone()));
    store.acquire_lock().expect("lock acquired on fresh store");

    let raw = fs::read_to_string(cfg.lock_path()).expect("lock.pid present");
    assert_eq!(raw.trim(), process::id().to_string());
    cleanup(&root);
}

#[test]
fn lockfile_released_on_drop() {
    let root = scratch_root("release");
    let cfg = cfg_under(&root);
    {
        let store = HandleStore::with_persist(64 * 1024, Some(cfg.clone()));
        store.acquire_lock().unwrap();
        assert!(cfg.lock_path().exists());
    }
    // Store dropped → LockGuard Drop deletes the file.
    assert!(!cfg.lock_path().exists(), "lock removed on drop");
    cleanup(&root);
}

#[test]
fn lockfile_rejects_concurrent_substrate() {
    let root = scratch_root("concurrent");
    let cfg = cfg_under(&root);
    let first = HandleStore::with_persist(64 * 1024, Some(cfg.clone()));
    first.acquire_lock().expect("first store acquires");

    // A second store against the same dir sees our own (live) PID and
    // refuses.
    let second = HandleStore::with_persist(64 * 1024, Some(cfg));
    let err = second.acquire_lock().expect_err("second store rejected");
    match err {
        LockError::Held { pid, .. } => {
            assert_eq!(pid, i32::try_from(process::id()).unwrap());
        }
        LockError::Io { path, error } => {
            panic!("expected Held, got Io({}): {error}", path.display())
        }
    }
    drop(first);
    cleanup(&root);
}

#[test]
fn lockfile_reclaims_stale_lock() {
    let root = scratch_root("stale");
    let cfg = cfg_under(&root);
    // Pre-seed a lock.pid with an almost-certainly-dead PID.
    fs::create_dir_all(&cfg.root).unwrap();
    fs::write(cfg.lock_path(), b"999999").unwrap();

    let store = HandleStore::with_persist(64 * 1024, Some(cfg.clone()));
    store.acquire_lock().expect("stale lock reclaimed");
    // Now holds our PID.
    let raw = fs::read_to_string(cfg.lock_path()).unwrap();
    assert_eq!(raw.trim(), process::id().to_string());
    cleanup(&root);
}

#[test]
fn lockfile_reclaims_garbage_lock() {
    let root = scratch_root("garbage");
    let cfg = cfg_under(&root);
    fs::create_dir_all(&cfg.root).unwrap();
    fs::write(cfg.lock_path(), b"not-a-pid").unwrap();

    let store = HandleStore::with_persist(64 * 1024, Some(cfg.clone()));
    store
        .acquire_lock()
        .expect("garbage lock reclaimed as stale");
    let raw = fs::read_to_string(cfg.lock_path()).unwrap();
    assert_eq!(raw.trim(), process::id().to_string());
    cleanup(&root);
}

#[test]
fn lockfile_does_not_reclaim_live_lock() {
    let root = scratch_root("live");
    let cfg = cfg_under(&root);
    fs::create_dir_all(&cfg.root).unwrap();
    // Our own PID — we're alive, so the lock must not be reclaimed.
    fs::write(cfg.lock_path(), process::id().to_string()).unwrap();

    let store = HandleStore::with_persist(64 * 1024, Some(cfg));
    let err = store.acquire_lock().expect_err("live lock not reclaimed");
    assert!(matches!(err, LockError::Held { .. }));
    cleanup(&root);
}

#[test]
fn lockfile_noop_when_persistence_disabled() {
    // No PersistConfig → acquire_lock is a no-op success.
    let store = HandleStore::new(64 * 1024);
    store.acquire_lock().expect("no-op when persistence off");
}
