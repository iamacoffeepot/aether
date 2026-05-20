//! Issue #984: on-disk layout + atomic write path for the persistent
//! handle store (ADR-0049 §2 + §3).
//!
//! Exercises `HandleStore::put_persistent`, the `<hash>.bin` /
//! `<hash>.meta` sidecar pair, the atomic tmp+rename writer, and the
//! `pinned.set` rewrite on pin / unpin. Store-mechanics tests construct
//! a `PersistConfig` directly against a unique scratch dir; the env-var
//! tests drive `PersistConfig::from_env` / a boot through the
//! persist-disabled chassis vote.

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
use std::time::{SystemTime, UNIX_EPOCH};

use aether_data::{HandleId, KindId};
use aether_substrate::handle_store::meta::{HandleMeta, SCHEMA_VERSION};
use aether_substrate::handle_store::{
    ENV_PERSIST_DISABLE, HandleStore, PersistConfig, entry_paths,
};

/// Process-global nonce so concurrent scratch dirs never collide even
/// when two tests start in the same millisecond.
static NONCE: AtomicU64 = AtomicU64::new(0);

fn scratch_root(tag: &str) -> PathBuf {
    let pid = process::id();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("aether-handle-persist-{tag}-{pid}-{millis}-{n}"));
    fs::create_dir_all(&path).expect("test setup: scratch dir creates");
    path
}

fn cleanup(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn persist_config(root: &Path) -> PersistConfig {
    PersistConfig {
        root: root.join("v1"),
        disk_budget_bytes: u64::MAX,
        eviction_tick_secs: 60,
    }
}

fn read_meta(path: &Path) -> HandleMeta {
    let bytes = fs::read(path).expect("meta file present");
    postcard::from_bytes(&bytes).expect("meta decodes")
}

#[test]
fn persist_writes_bin_and_meta() {
    let root = scratch_root("bin-meta");
    let cfg = persist_config(&root);
    let store = HandleStore::with_persist(1024 * 1024, Some(cfg.clone()));

    let id = HandleId(0x1234_5678);
    let kind = KindId(0xAAAA);
    let bytes = b"hello-world".to_vec();
    store.put_persistent(id, kind, bytes.clone(), None).unwrap();

    let (bin_path, meta_path) = entry_paths(&cfg.root, id);
    assert!(bin_path.exists(), "bin written");
    assert!(meta_path.exists(), "meta written");
    assert_eq!(fs::read(&bin_path).unwrap(), bytes);

    let meta = read_meta(&meta_path);
    assert_eq!(meta.schema_version, SCHEMA_VERSION);
    assert_eq!(meta.handle_id, id.0);
    assert_eq!(meta.kind_id, kind.0);
    assert_eq!(meta.bytes_len as usize, bytes.len());
    assert!(!meta.pinned);
    assert!(meta.transform_origin.is_none());

    cleanup(&root);
}

#[test]
fn persist_atomic_under_concurrent_write() {
    let root = scratch_root("atomic");
    let cfg = persist_config(&root);
    let store = Arc::new(HandleStore::with_persist(1024 * 1024, Some(cfg.clone())));

    // Content-addressed: ten threads produce the same handle with the
    // same bytes (two transforms producing the same output).
    let id = HandleId(0xCAFE);
    let kind = KindId(0xBEEF);
    let bytes = b"deterministic-payload".to_vec();

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let store = Arc::clone(&store);
            let bytes = bytes.clone();
            thread::spawn(move || {
                store.put_persistent(id, kind, bytes, None).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let (bin_path, meta_path) = entry_paths(&cfg.root, id);
    // Exactly one final bin + meta, no leftover tmp files.
    let shard_dir = bin_path.parent().unwrap();
    let entries: Vec<String> = fs::read_dir(shard_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !entries.iter().any(|e| e.contains(".tmp-")),
        "no tmp files left behind: {entries:?}",
    );
    assert_eq!(fs::read(&bin_path).unwrap(), bytes);
    let meta = read_meta(&meta_path);
    assert_eq!(meta.bytes_len as usize, bytes.len());

    cleanup(&root);
}

#[test]
fn persist_writes_pinned_set_on_pin() {
    let root = scratch_root("pin");
    let cfg = persist_config(&root);
    let store = HandleStore::with_persist(1024 * 1024, Some(cfg.clone()));

    let id = HandleId(0x42);
    store
        .put_persistent(id, KindId(1), b"x".to_vec(), None)
        .unwrap();
    assert!(store.pin(id));

    let raw = fs::read(cfg.pinned_set_path()).expect("pinned.set present");
    let ids: Vec<u64> = raw
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert!(ids.contains(&id.0), "pinned set contains the id: {ids:?}");

    cleanup(&root);
}

#[test]
fn persist_writes_pinned_set_on_unpin() {
    let root = scratch_root("unpin");
    let cfg = persist_config(&root);
    let store = HandleStore::with_persist(1024 * 1024, Some(cfg.clone()));

    let id = HandleId(0x99);
    store
        .put_persistent(id, KindId(1), b"x".to_vec(), None)
        .unwrap();
    store.pin(id);
    store.unpin(id);

    let raw = fs::read(cfg.pinned_set_path()).expect("pinned.set present");
    let ids: Vec<u64> = raw
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert!(!ids.contains(&id.0), "id removed from pinned set: {ids:?}");

    cleanup(&root);
}

#[test]
fn persist_disabled_when_no_config() {
    // No PersistConfig — `put_persistent` behaves as an in-memory put.
    let root = scratch_root("disabled");
    let store = HandleStore::with_persist(1024 * 1024, None);
    let id = HandleId(0x7);
    store
        .put_persistent(id, KindId(1), b"x".to_vec(), None)
        .unwrap();

    assert!(store.contains(id), "in-memory entry present");
    // Nothing written to the scratch dir (we never handed it the dir).
    assert!(
        fs::read_dir(&root).unwrap().next().is_none(),
        "no files written when persistence is off",
    );
    cleanup(&root);
}

#[test]
fn persist_config_from_env_disable_flag() {
    // The disable flag wins even when the chassis votes enabled. This
    // test mutates a process-global env var; nextest runs each test in
    // its own process so the mutation is isolated.
    let prev = env::var(ENV_PERSIST_DISABLE).ok();
    // SAFETY: single-threaded test body; nextest isolates the process.
    unsafe {
        env::set_var(ENV_PERSIST_DISABLE, "1");
    }
    assert!(
        PersistConfig::from_env(true).is_none(),
        "disable flag forces None even with chassis vote true",
    );
    // Chassis vote false is always None.
    assert!(PersistConfig::from_env(false).is_none());
    // SAFETY: restore.
    unsafe {
        match prev {
            Some(v) => env::set_var(ENV_PERSIST_DISABLE, v),
            None => env::remove_var(ENV_PERSIST_DISABLE),
        }
    }
}

#[test]
fn persist_continues_on_write_failure() {
    // Point the config root at a path whose parent is a regular file,
    // so create_dir_all fails and the disk write errors. The in-memory
    // entry must still succeed (best-effort persistence, ADR-0049 §3).
    let root = scratch_root("write-fail");
    // Make `root/v1` collide with a file so entries/ can't be created
    // under it.
    let blocker = root.join("v1");
    fs::write(&blocker, b"not-a-dir").unwrap();
    let cfg = PersistConfig {
        root: blocker,
        disk_budget_bytes: u64::MAX,
        eviction_tick_secs: 60,
    };
    let store = HandleStore::with_persist(1024 * 1024, Some(cfg));

    let id = HandleId(0x55);
    // Does NOT return an error — the put succeeds, the disk write warns.
    store
        .put_persistent(id, KindId(1), b"payload".to_vec(), None)
        .unwrap();
    assert!(store.contains(id), "in-memory entry survives disk failure");

    cleanup(&root);
}
