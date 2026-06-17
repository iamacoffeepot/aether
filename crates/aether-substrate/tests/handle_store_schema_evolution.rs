//! Issue #988: schema-evolution invalidation via `kind_id` (ADR-0049 §6).
//!
//! On restore, the boot scan compares each entry's `meta.kind_id`
//! against the current registry's id for the same `kind_name`. A
//! mismatch (schema changed), a missing name (kind retired), or an
//! unsupported `schema_version` invalidates the entry: both files are
//! deleted and it's treated as a cache miss. The invalidation overrides
//! pin — schema-stale bytes are unsafe to decode, so a pinned entry is
//! still evicted on mismatch.

#![allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic-on-failure is the assertion"
)]

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use aether_data::wire;
use aether_data::{HandleId, KindId};
use aether_substrate::handle_store::meta::{HandleMeta, TransformOrigin};
use aether_substrate::handle_store::{HandleStore, KindResolver, PersistConfig, entry_paths};

static NONCE: AtomicU64 = AtomicU64::new(0);

fn scratch_root(tag: &str) -> PathBuf {
    let pid = process::id();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("aether-handle-schema-{tag}-{pid}-{millis}-{n}"));
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

/// A synthetic kind registry: bidirectional name <-> id, for driving
/// the schema-evolution check without standing up a real `Registry`.
struct FakeRegistry {
    by_name: HashMap<String, KindId>,
    by_id: HashMap<KindId, String>,
}

impl FakeRegistry {
    fn new(entries: &[(&str, u64)]) -> Self {
        let mut by_name = HashMap::new();
        let mut by_id = HashMap::new();
        for (name, id) in entries {
            by_name.insert((*name).to_owned(), KindId(*id));
            by_id.insert(KindId(*id), (*name).to_owned());
        }
        Self { by_name, by_id }
    }
}

impl KindResolver for FakeRegistry {
    fn id_for_name(&self, name: &str) -> Option<KindId> {
        self.by_name.get(name).copied()
    }
    fn name_for_id(&self, id: KindId) -> Option<String> {
        self.by_id.get(&id).cloned()
    }
}

fn store_with(cfg: &PersistConfig, registry: &[(&str, u64)]) -> HandleStore {
    let resolver: Arc<dyn KindResolver> = Arc::new(FakeRegistry::new(registry));
    HandleStore::with_persist_validated(64 * 1024 * 1024, Some(cfg.clone()), Some(resolver))
}

#[test]
fn schema_evolution_evicts_changed_kind_id() {
    let root = scratch_root("changed");
    let cfg = cfg_under(&root);
    let id = HandleId(1);
    {
        // Write under kind "demo.kind" = 0xAAAA.
        let store = store_with(&cfg, &[("demo.kind", 0xAAAA)]);
        store
            .put_persistent(id, KindId(0xAAAA), b"bytes".to_vec(), None)
            .unwrap();
    }
    // Restart with the registry returning 0xBBBB for the same name.
    let restored = store_with(&cfg, &[("demo.kind", 0xBBBB)]);
    assert_eq!(restored.disk_index_len(), 0, "stale entry dropped");
    let (bin, meta) = entry_paths(&cfg.root, id);
    assert!(!bin.exists() && !meta.exists(), "both files deleted");
    cleanup(&root);
}

#[test]
fn schema_evolution_evicts_retired_kind() {
    let root = scratch_root("retired");
    let cfg = cfg_under(&root);
    let id = HandleId(2);
    {
        let store = store_with(&cfg, &[("demo.kind", 0xAAAA)]);
        store
            .put_persistent(id, KindId(0xAAAA), b"bytes".to_vec(), None)
            .unwrap();
    }
    // Restart with an empty registry — kind retired.
    let restored = store_with(&cfg, &[]);
    assert_eq!(restored.disk_index_len(), 0, "retired-kind entry dropped");
    let (bin, meta) = entry_paths(&cfg.root, id);
    assert!(!bin.exists() && !meta.exists());
    cleanup(&root);
}

#[test]
fn schema_evolution_keeps_matching_kind() {
    let root = scratch_root("match");
    let cfg = cfg_under(&root);
    let id = HandleId(3);
    {
        let store = store_with(&cfg, &[("demo.kind", 0xAAAA)]);
        store
            .put_persistent(id, KindId(0xAAAA), b"bytes".to_vec(), None)
            .unwrap();
    }
    // Restart with the same id — entry survives.
    let restored = store_with(&cfg, &[("demo.kind", 0xAAAA)]);
    assert_eq!(restored.disk_index_len(), 1, "matching entry kept");
    assert_eq!(restored.get(id).map(|(_, b)| b), Some(b"bytes".to_vec()));
    cleanup(&root);
}

#[test]
fn schema_evolution_evicts_old_schema_version() {
    let root = scratch_root("old-version");
    let cfg = cfg_under(&root);
    let id = HandleId(4);
    // Hand-craft a schema_version = 1 meta + a bin (a pre-v2 entry).
    let (bin, meta_path) = entry_paths(&cfg.root, id);
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::write(&bin, b"legacy").unwrap();
    // wire-encode a meta that claims version 1.
    let meta = HandleMeta {
        schema_version: 1,
        handle_id: id.0,
        kind_id: 0xAAAA,
        kind_name: "demo.kind".to_owned(),
        transform_origin: None,
        bytes_len: 6,
        created_at: 1,
        pinned: false,
    };
    fs::write(&meta_path, wire::to_vec(&meta).unwrap()).unwrap();

    let restored = store_with(&cfg, &[("demo.kind", 0xAAAA)]);
    assert_eq!(restored.disk_index_len(), 0, "version-1 entry evicted");
    assert!(!bin.exists() && !meta_path.exists());
    cleanup(&root);
}

#[test]
fn schema_evolution_pinned_entry_still_invalidates() {
    // Contract (ADR-0049 §6): pin protects against budget pressure, not
    // against schema-mismatch correctness eviction. A pinned entry whose
    // kind id changed is still evicted.
    let root = scratch_root("pinned-invalidates");
    let cfg = cfg_under(&root);
    let id = HandleId(5);
    {
        let store = store_with(&cfg, &[("demo.kind", 0xAAAA)]);
        let origin = TransformOrigin {
            component_mailbox: 1,
            transform_index: 0,
            input_handle_ids: vec![],
        };
        store
            .put_persistent(id, KindId(0xAAAA), b"bytes".to_vec(), Some(origin))
            .unwrap();
        store.pin(id);
    }
    // Restart with a changed id — the pinned entry is still evicted.
    let restored = store_with(&cfg, &[("demo.kind", 0xBBBB)]);
    assert_eq!(
        restored.disk_index_len(),
        0,
        "pinned-but-stale entry dropped"
    );
    let (bin, meta) = entry_paths(&cfg.root, id);
    assert!(!bin.exists() && !meta.exists());
    cleanup(&root);
}
