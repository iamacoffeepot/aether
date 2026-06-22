//! Content-addressed artifact store for the hub (ADR-0115, issue 1953).
//!
//! The storage half of the hub artifact registry: somewhere above the
//! engine layer to keep uploaded binaries content-addressed, ingest one
//! from a staged host path, and read what each binary *is*. The store is
//! hub-scoped and keyed on a sha256 over the raw bytes, so an identical
//! re-upload dedups to the same entry.
//!
//! Artifact-generic from the start — an entry is a content blob plus a
//! type-tagged ([`ArtifactKind`]) manifest — so the registry extraction
//! (#1955) lifts it whole. Two artifact types share the store: a chassis
//! binary (a [`BinaryManifest`](aether_kinds::BinaryManifest), ADR-0115)
//! and a wasm component (a
//! [`ComponentManifest`](aether_kinds::ComponentManifest) read straight
//! from the wasm, ADR-0116 / #1956),
//! carried in the [`StoredManifest`] enum.
//!
//! ## Layout
//!
//! Under a hub-scoped, layout-versioned root — the dir resolved from
//! `EngineConfig`'s `binary_store_dir` field (the `AETHER_BINARY_STORE_DIR`
//! env layer, ADR-0090) or the computed default `data_dir/aether/binaries/v1`:
//!
//! ```text
//! <root>/
//!   entries/
//!     <hash>            the raw bytes (content-addressed)
//!     <hash>.manifest   the type tag + manifest, JSON
//!   names.json          name -> hash map
//!   lock.pid            owning-process pid (best-effort reclaim)
//! ```
//!
//! The store survives a `restart-hub` because the root persists across the
//! hub child's restart. The disk budget is enforced by LRU eviction over
//! entries that are neither pinned nor named — a named or pinned entry is
//! kept regardless of recency.
//!
//! The `lock.pid` acquisition protocol lives in
//! `aether_substrate::pid_lock`. The `aether.engine` cap is
//! single-threaded (one dispatcher run-token), so the store holds its
//! index in plain fields behind `&mut self` rather than an inner lock.

mod eviction;
mod manifest;
mod persistence;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use aether_kinds::{BinaryEntry, ComponentEntry, ListComponentBinaries, ListEngineBinaries};
use aether_substrate::atomic_write::atomic_write;
use aether_substrate::pid_lock::LockGuard;
use serde::{Deserialize, Serialize};

pub use manifest::{ArtifactKind, Selector, StoredArtifact, StoredManifest, component_manifest};
use manifest::{matches_binary_filter, matches_component_filter};
use persistence::{RestoredIndex, acquire_lock, ensure_root, hash_hex, restore, write_sidecar};

/// Layout-version subdirectory under the resolved root, so a future
/// on-disk format change can land beside `v1` without a migration.
pub const LAYOUT_VERSION_DIR: &str = "v1";

/// Default on-disk byte budget. 16 GiB; binaries are tens of megabytes,
/// so this holds a deep history before LRU eviction kicks in.
/// `EngineConfig`'s `binary_disk_budget_bytes`
/// carries this as its literal default (`17_179_869_184`) and folds an
/// unparseable env value back to it.
pub const DEFAULT_DISK_BUDGET_BYTES: u64 = 16 * 1024 * 1024 * 1024;

const TARGET: &str = "aether_capabilities::store";

/// The JSON sidecar written next to each entry's bytes — the type tag
/// plus the type-tagged manifest, so a fresh store rebuilds its index
/// from disk. `kind` is redundant with the `manifest` variant but kept
/// for a forward-compatible read of an entry whose manifest variant a
/// future build doesn't recognize.
#[derive(Serialize, Deserialize, Clone)]
struct StoredEntry {
    kind: ArtifactKind,
    manifest: StoredManifest,
}

/// In-memory record of one entry. The bytes live on disk at
/// `entries/<hash>`; only the metadata is held in memory (artifacts are
/// large), read back lazily on [`ArtifactStore::get`].
struct Entry {
    kind: ArtifactKind,
    manifest: StoredManifest,
    bytes_len: u64,
    /// Eviction protection independent of naming (an explicit
    /// [`ArtifactStore::pin`]). A named entry is also eviction-protected.
    pinned: bool,
    /// Monotonic access stamp; lower = older, the LRU eviction key.
    last_access: u64,
}

/// Content-addressed, disk-backed, budget-bounded artifact store
/// (ADR-0115). Owned by the single-threaded `aether.engine` cap, so its
/// index lives in plain fields behind `&mut self`; #1955 can wrap it
/// behind a lock when a multi-owner registry needs one.
pub struct ArtifactStore {
    /// The layout-versioned root holding `entries/`, `names.json`,
    /// `lock.pid`.
    root: PathBuf,
    disk_budget_bytes: u64,
    /// hash -> entry metadata.
    entries: HashMap<String, Entry>,
    /// name -> hash. Repointing a name to a new hash is a plain overwrite;
    /// the old hash keeps its bytes but loses its name (and so its
    /// eviction protection).
    names: HashMap<String, String>,
    /// Approximate on-disk byte ledger, the LRU eviction trigger.
    total_bytes: u64,
    /// Monotonic source for `Entry::last_access`.
    clock: u64,
    /// `lock.pid` guard. Held for the store's lifetime when the lock was
    /// freshly written; `None` when another live process holds it (the
    /// store still operates — a content-addressed store tolerates a shared
    /// dir, so the lock is hygiene, not a hard mutex).
    _lock: Option<LockGuard>,
}

impl ArtifactStore {
    /// The computed default layout root for the store — `data_dir`'s
    /// `aether/binaries/<LAYOUT_VERSION_DIR>`, or a `temp_dir` fallback
    /// when no platform data dir resolves. No env read: the
    /// `AETHER_BINARY_STORE_DIR` override now rides `EngineConfig`'s
    /// `binary_store_dir` field (ADR-0090), and `EngineServer::init` joins
    /// [`LAYOUT_VERSION_DIR`] to a configured override or falls back here
    /// when it's unset.
    #[must_use]
    pub fn default_root() -> PathBuf {
        if let Some(data) = dirs::data_dir() {
            return data
                .join("aether")
                .join("binaries")
                .join(LAYOUT_VERSION_DIR);
        }
        env::temp_dir()
            .join("aether-binaries")
            .join(LAYOUT_VERSION_DIR)
    }

    /// Open (or create) the store at `root` with the given disk budget.
    /// Infallible: a root that can't be created falls back to a unique
    /// temp dir so the hub always comes up with a working store. The
    /// `lock.pid` reclaim is best-effort — a stale (dead-pid) or garbage
    /// lock is reclaimed; a live holder leaves the store unlocked but
    /// still operating.
    #[must_use]
    pub fn open(root: &Path, disk_budget_bytes: u64) -> Self {
        let root = ensure_root(root);
        let lock = acquire_lock(&root);
        let RestoredIndex {
            entries,
            names,
            total_bytes,
            clock,
        } = restore(&root);
        Self {
            root,
            disk_budget_bytes,
            entries,
            names,
            total_bytes,
            clock,
            _lock: lock,
        }
    }

    /// The layout root this store resolved to (after any temp fallback).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ingest `bytes` content-addressed, recording `manifest` and
    /// (optionally) pointing `name` at the resulting hash. A re-upload of
    /// identical bytes dedups: the bytes aren't rewritten and the same
    /// hash comes back, but a fresh `name` still repoints. Returns the
    /// sha256 hex the bytes stored under. Runs LRU eviction afterward to
    /// hold the disk budget.
    pub fn upload(
        &mut self,
        bytes: &[u8],
        kind: ArtifactKind,
        manifest: StoredManifest,
        name: Option<String>,
    ) -> String {
        let hash = hash_hex(bytes);
        let clock = self.next_clock();

        if let Some(entry) = self.entries.get_mut(&hash) {
            // Dedup: bump recency so a re-uploaded entry isn't the first
            // eviction target.
            entry.last_access = clock;
        } else {
            // New content: write the bytes + the sidecar, then index it. A
            // write failure leaves the entry out of the index, so the store
            // stays consistent — the next upload of the same bytes retries.
            let (bytes_path, manifest_path) = self.entry_paths(&hash);
            let sidecar = StoredEntry {
                kind,
                manifest: manifest.clone(),
            };
            if let Err(e) = atomic_write(&bytes_path, bytes) {
                tracing::warn!(target: TARGET, hash = %hash, error = %e, "binary store: writing entry bytes failed");
            } else if let Err(e) = write_sidecar(&manifest_path, &sidecar) {
                tracing::warn!(target: TARGET, hash = %hash, error = %e, "binary store: writing entry manifest failed");
                let _ = fs::remove_file(&bytes_path);
            } else {
                let bytes_len = bytes.len() as u64;
                self.entries.insert(
                    hash.clone(),
                    Entry {
                        kind,
                        manifest,
                        bytes_len,
                        pinned: false,
                        last_access: clock,
                    },
                );
                self.total_bytes = self.total_bytes.saturating_add(bytes_len);
            }
        }

        if let Some(name) = name {
            self.names.insert(name, hash.clone());
            self.persist_names();
        }

        self.evict_if_needed();
        hash
    }

    /// Pin (or unpin) an entry by hash, protecting it from eviction
    /// independent of whether a name points at it. Returns `false` if no
    /// entry has that hash. Persistence of the pin flag is a fast-follow —
    /// today a pin holds for the store's lifetime (the hub process).
    pub fn set_pinned(&mut self, hash: &str, pinned: bool) -> bool {
        if let Some(entry) = self.entries.get_mut(hash) {
            entry.pinned = pinned;
            true
        } else {
            false
        }
    }

    /// Pin an entry by hash. Convenience for `set_pinned(hash, true)`.
    pub fn pin(&mut self, hash: &str) -> bool {
        self.set_pinned(hash, true)
    }

    /// Enumerate the stored binaries matching `filter` as
    /// [`BinaryEntry`]s. The filter fields are AND-combined: `chassis` /
    /// `target` are exact matches, `caps` requires the entry's caps to be
    /// a superset of every listed cap. Each absent field is "no
    /// constraint". Component entries are excluded — only `Binary`-kind
    /// artifacts are listed here.
    #[must_use]
    pub fn list_binaries(&self, filter: &ListEngineBinaries) -> Vec<BinaryEntry> {
        self.entries
            .iter()
            .filter_map(|(hash, entry)| {
                let manifest = entry.manifest.as_binary()?;
                matches_binary_filter(manifest, filter).then(|| BinaryEntry {
                    hash: hash.clone(),
                    name: self.name_for(hash),
                    manifest: manifest.clone(),
                })
            })
            .collect()
    }

    /// Enumerate the stored components matching `filter` as
    /// [`ComponentEntry`]s (ADR-0116, issue 1956). The filter fields are
    /// AND-combined: `namespace` keeps entries exporting that actor
    /// namespace, `handled_kind` keeps entries handling that `KindId`.
    /// Each absent field is "no constraint". Binary entries are excluded —
    /// only `Component`-kind artifacts are listed here.
    #[must_use]
    pub fn list_components(&self, filter: &ListComponentBinaries) -> Vec<ComponentEntry> {
        self.entries
            .iter()
            .filter_map(|(hash, entry)| {
                let manifest = entry.manifest.as_component()?;
                matches_component_filter(manifest, filter).then(|| ComponentEntry {
                    hash: hash.clone(),
                    name: self.name_for(hash),
                    manifest: manifest.clone(),
                })
            })
            .collect()
    }

    /// Resolve an artifact by hash or name to its on-disk path + manifest
    /// (ADR-0115; the seam #1954 consumes). `None` if the hash / name
    /// isn't stored. Bumps the entry's recency.
    pub fn get(&mut self, selector: &Selector) -> Option<StoredArtifact> {
        let hash = match selector {
            Selector::Hash(h) => h.clone(),
            Selector::Name(n) => self.names.get(n)?.clone(),
        };
        let clock = self.next_clock();
        let entry = self.entries.get_mut(&hash)?;
        entry.last_access = clock;
        let kind = entry.kind;
        let manifest = entry.manifest.clone();
        let name = self.name_for(&hash);
        let (path, _) = self.entry_paths(&hash);
        Some(StoredArtifact {
            hash,
            path,
            kind,
            manifest,
            name,
        })
    }

    /// Number of stored entries.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Approximate on-disk byte total.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Whether an entry with `hash` is stored.
    #[must_use]
    pub fn contains(&self, hash: &str) -> bool {
        self.entries.contains_key(hash)
    }

    /// The first name pointing at `hash`, for a [`BinaryEntry`] / a
    /// [`StoredArtifact`]. At most one name per hash in practice.
    fn name_for(&self, hash: &str) -> Option<String> {
        self.names
            .iter()
            .find(|(_, h)| h.as_str() == hash)
            .map(|(n, _)| n.clone())
    }

    fn next_clock(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The `(bytes, manifest-sidecar)` paths for `hash` under `entries/`.
    fn entry_paths(&self, hash: &str) -> (PathBuf, PathBuf) {
        let dir = self.root.join("entries");
        (dir.join(hash), dir.join(format!("{hash}.manifest")))
    }

    fn names_path(&self) -> PathBuf {
        self.root.join("names.json")
    }

    /// Rewrite `names.json` from the in-memory map (best-effort).
    fn persist_names(&self) {
        match serde_json::to_vec(&self.names) {
            Ok(bytes) => {
                if let Err(e) = atomic_write(&self.names_path(), &bytes) {
                    tracing::warn!(target: TARGET, error = %e, "binary store: persisting names failed");
                }
            }
            Err(e) => {
                tracing::warn!(target: TARGET, error = %e, "binary store: encoding names failed");
            }
        }
    }
}
