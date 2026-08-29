//! Content-addressed artifact store for the hub (ADR-0115, issue 1953).
//!
//! Private implementation detail of the `aether.fleet` cap: a plain
//! struct held as one field of `FleetServer`, keeping uploaded binaries
//! content-addressed, ingesting one from a staged host path, and reading
//! what each binary *is*. The store is hub-scoped and keyed on a sha256
//! over the raw bytes, so an identical re-upload dedups to the same entry.
//!
//! Artifact-generic by design — an entry is a content blob plus a
//! type-tagged ([`ArtifactKind`]) manifest. Two artifact types share the
//! store: a chassis binary (a
//! [`BinaryManifest`](aether_kinds::BinaryManifest), ADR-0115) and a wasm
//! component (a [`ComponentManifest`](aether_kinds::ComponentManifest)
//! read straight from the wasm, ADR-0116 / #1956), carried in the
//! [`StoredManifest`] enum.
//!
//! ## Storage core (ADR-0149)
//!
//! The domain-clean storage layer — sha256 addressing, the on-disk entry
//! index, atomic sidecar persistence + restore, `lock.pid` acquisition,
//! and the eviction step — lives in
//! [`aether_substrate::content_store`] as [`ContentStore<M>`], parameterized
//! over the per-entry metadata type and an [`EvictionPolicy`]. This module
//! is one consumer of it: [`ArtifactStore`] wraps a
//! `ContentStore<StoredEntry>` under the [`LruBudget`](EvictionPolicy::LruBudget)
//! policy and layers the binary/component vocabulary — [`ArtifactKind`] /
//! [`StoredManifest`], the manifest filters, and the four list/match
//! projections — over the core's entry-iteration API. Bloomery's
//! eviction-free `artifacts` port (ADR-0149) is the second consumer.
//!
//! ## Layout
//!
//! Under a hub-scoped, layout-versioned root — the dir resolved from
//! `FleetConfig`'s `binary_store_dir` field (the `AETHER_BINARY_STORE_DIR`
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
//! kept regardless of recency. Both protections are on disk: a name in
//! `names.json`, a pin in the entry's own sidecar, so neither lapses when
//! the supervised hub restarts.

mod manifest;
#[cfg(test)]
mod tests;

use std::env;
use std::io;
use std::path::{Path, PathBuf};

use aether_kinds::{
    BinaryEntry, ComponentEntry, ListComponentBinaries, ListComponentBinariesResult, ListEngineBinaries,
    ListEngineBinariesResult,
};
use aether_substrate::content_store::{ContentStore, EvictionPolicy};
use serde::{Deserialize, Serialize};

pub use aether_substrate::content_store::Selector;
pub use manifest::{ArtifactKind, StoredArtifact, StoredManifest, component_manifest, config_descriptor};
use manifest::{matches_binary_filter, matches_component_filter};

/// The core's `now_nanos`, re-surfaced under the pre-extraction path so the
/// in-tree regression suite (`tests.rs`) keeps its temp-root nonce helper
/// unchanged — the bit-for-bit gate must not be edited by the move.
#[cfg(test)]
mod persistence {
    pub use aether_substrate::content_store::now_nanos;
}

/// Layout-version subdirectory under the resolved root, so a future
/// on-disk format change can land beside `v1` without a migration.
pub const LAYOUT_VERSION_DIR: &str = "v1";

/// Default on-disk byte budget. 16 GiB; binaries are tens of megabytes,
/// so this holds a deep history before LRU eviction kicks in.
/// `FleetConfig`'s `binary_disk_budget_bytes`
/// carries this as its literal default (`17_179_869_184`) and folds an
/// unparseable env value back to it.
pub const DEFAULT_DISK_BUDGET_BYTES: u64 = 16 * 1024 * 1024 * 1024;

const DEFAULT_LIST_CAP: u32 = 20;

/// The hub's per-entry metadata `M` for [`ContentStore`]: the type tag
/// plus the type-tagged manifest. The core flattens this beside its own
/// entry state, so the on-disk sidecar is
/// `{ kind, manifest, uploaded_seq, pinned }` — the pre-extraction JSON
/// layout plus the two store-owned fields, each of which defaults on read
/// so an older sidecar still restores. `kind` is redundant with the
/// `manifest` variant but kept for a forward-compatible read of an entry
/// whose manifest variant a future build doesn't recognize.
#[derive(Serialize, Deserialize, Clone)]
pub struct StoredEntry {
    kind: ArtifactKind,
    manifest: StoredManifest,
}

/// Content-addressed, disk-backed, budget-bounded artifact store
/// (ADR-0115). The hub consumer over [`ContentStore<StoredEntry>`] with the
/// [`LruBudget`](EvictionPolicy::LruBudget) policy: the core owns storage,
/// this layer owns the binary/component vocabulary.
pub struct ArtifactStore {
    inner: ContentStore<StoredEntry>,
}

impl ArtifactStore {
    /// The computed default layout root for the store — `data_dir`'s
    /// `aether/binaries/<LAYOUT_VERSION_DIR>`, or a `temp_dir` fallback
    /// when no platform data dir resolves. No env read: the
    /// `AETHER_BINARY_STORE_DIR` override now rides `FleetConfig`'s
    /// `binary_store_dir` field (ADR-0090), and `FleetServer::init` joins
    /// [`LAYOUT_VERSION_DIR`] to a configured override or falls back here
    /// when it's unset. Hub-domain naming, so it stays hub-side glue rather
    /// than moving into the domain-neutral core (ADR-0149 §Affected surfaces).
    #[must_use]
    pub fn default_root() -> PathBuf {
        if let Some(data) = dirs::data_dir() {
            return data.join("aether").join("binaries").join(LAYOUT_VERSION_DIR);
        }
        env::temp_dir().join("aether-binaries").join(LAYOUT_VERSION_DIR)
    }

    /// Open (or create) the store at `root` with the given disk budget. A
    /// root that can't be created falls back to a unique temp dir, so the
    /// hub normally comes up with a working store; a total storage failure
    /// (configured root and temp fallback both uncreatable) surfaces as an
    /// error. The `lock.pid` reclaim is best-effort — a stale (dead-pid)
    /// or garbage lock is reclaimed; a live holder leaves the store
    /// unlocked but still operating.
    pub fn open(root: &Path, disk_budget_bytes: u64) -> io::Result<Self> {
        Ok(Self { inner: ContentStore::open(root, EvictionPolicy::LruBudget(disk_budget_bytes))? })
    }

    /// The layout root this store resolved to (after any temp fallback).
    #[must_use]
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        self.inner.root()
    }

    /// Ingest `bytes` content-addressed, recording `manifest` and
    /// (optionally) pointing `name` at the resulting hash. A re-upload of
    /// identical bytes dedups: the bytes aren't rewritten and the same
    /// hash comes back, but a fresh `name` still repoints. Returns the
    /// sha256 hex the bytes stored under. Runs LRU eviction afterward to
    /// hold the disk budget.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] when the artifact can't be
    /// persisted. Nothing is indexed and no name is repointed, so a
    /// selector built from the would-be hash would never resolve.
    pub fn upload(
        &mut self,
        bytes: &[u8],
        kind: ArtifactKind,
        manifest: StoredManifest,
        name: Option<String>,
    ) -> io::Result<String> {
        self.inner.upload(bytes, StoredEntry { kind, manifest }, name)
    }

    /// Pin (or unpin) an entry by hash, protecting it from eviction
    /// independent of whether a name points at it. Returns `false` if no
    /// entry has that hash. The flag is persisted in the entry's sidecar,
    /// so a pin survives a `restart-hub` rather than lapsing with the hub
    /// process that set it.
    #[allow(dead_code)]
    pub fn set_pinned(&mut self, hash: &str, pinned: bool) -> bool {
        self.inner.set_pinned(hash, pinned)
    }

    /// Pin an entry by hash. Convenience for `set_pinned(hash, true)`.
    #[allow(dead_code)]
    pub fn pin(&mut self, hash: &str) -> bool {
        self.inner.pin(hash)
    }

    /// Enumerate the stored binaries matching `filter` as
    /// [`BinaryEntry`]s. The filter fields are AND-combined: `chassis` /
    /// `target` are exact matches, `caps` requires the entry's caps to be
    /// a superset of every listed cap. Each absent field is "no
    /// constraint". Component entries are excluded — only `Binary`-kind
    /// artifacts are listed here.
    #[must_use]
    pub fn matching_binaries(&self, filter: &ListEngineBinaries) -> Vec<BinaryEntry> {
        self.inner
            .entries()
            .filter_map(|entry| {
                let manifest = entry.metadata.manifest.as_binary()?;
                matches_binary_filter(manifest, filter).then(|| BinaryEntry {
                    hash: entry.hash.to_owned(),
                    name: self.inner.name_for(entry.hash),
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
    pub fn matching_components(&self, filter: &ListComponentBinaries) -> Vec<ComponentEntry> {
        self.inner
            .entries()
            .filter_map(|entry| {
                let manifest = entry.metadata.manifest.as_component()?;
                matches_component_filter(manifest, filter).then(|| ComponentEntry {
                    hash: entry.hash.to_owned(),
                    name: self.inner.name_for(entry.hash),
                    manifest: manifest.clone(),
                })
            })
            .collect()
    }

    /// Return a consumer-facing page of stored binaries. Attribute filters
    /// are applied before the named/history choice; `total_matched` is
    /// recorded before truncation. Entries sort by stable first-ingest
    /// sequence descending, then hash ascending for deterministic legacy
    /// sequence ties.
    #[must_use]
    pub fn list_binaries_page(&self, filter: &ListEngineBinaries) -> ListEngineBinariesResult {
        let mut matches: Vec<_> = self
            .inner
            .entries()
            .filter_map(|entry| {
                let manifest = entry.metadata.manifest.as_binary()?;
                if !matches_binary_filter(manifest, filter) {
                    return None;
                }
                let name = self.inner.name_for(entry.hash);
                if name.is_none() && !filter.include_history {
                    return None;
                }
                Some((
                    entry.uploaded_seq,
                    BinaryEntry { hash: entry.hash.to_owned(), name, manifest: manifest.clone() },
                ))
            })
            .collect();
        matches.sort_by(|(left_seq, left), (right_seq, right)| {
            right_seq.cmp(left_seq).then_with(|| left.hash.cmp(&right.hash))
        });
        let total_matched = u32::try_from(matches.len()).unwrap_or(u32::MAX);
        matches.truncate(usize::try_from(filter.limit.unwrap_or(DEFAULT_LIST_CAP)).unwrap_or(usize::MAX));
        ListEngineBinariesResult { binaries: matches.into_iter().map(|(_, entry)| entry).collect(), total_matched }
    }

    /// Return a consumer-facing page of stored components with the same
    /// filtering, stable ordering, and pre-truncation count contract as
    /// [`ArtifactStore::list_binaries_page`].
    #[must_use]
    pub fn list_components_page(&self, filter: &ListComponentBinaries) -> ListComponentBinariesResult {
        let mut matches: Vec<_> = self
            .inner
            .entries()
            .filter_map(|entry| {
                let manifest = entry.metadata.manifest.as_component()?;
                if !matches_component_filter(manifest, filter) {
                    return None;
                }
                let name = self.inner.name_for(entry.hash);
                if name.is_none() && !filter.include_history {
                    return None;
                }
                Some((
                    entry.uploaded_seq,
                    ComponentEntry { hash: entry.hash.to_owned(), name, manifest: manifest.clone() },
                ))
            })
            .collect();
        matches.sort_by(|(left_seq, left), (right_seq, right)| {
            right_seq.cmp(left_seq).then_with(|| left.hash.cmp(&right.hash))
        });
        let total_matched = u32::try_from(matches.len()).unwrap_or(u32::MAX);
        matches.truncate(usize::try_from(filter.limit.unwrap_or(DEFAULT_LIST_CAP)).unwrap_or(usize::MAX));
        ListComponentBinariesResult { components: matches.into_iter().map(|(_, entry)| entry).collect(), total_matched }
    }

    /// Resolve an artifact by hash or name to its on-disk path + manifest
    /// (ADR-0115; the seam #1954 consumes). `None` if the hash / name
    /// isn't stored. Bumps the entry's recency.
    pub fn get(&mut self, selector: &Selector) -> Option<StoredArtifact> {
        let resolved = self.inner.get(selector)?;
        Some(StoredArtifact {
            hash: resolved.hash,
            path: resolved.path,
            kind: resolved.metadata.kind,
            manifest: resolved.metadata.manifest,
            name: resolved.name,
        })
    }

    /// Number of stored entries.
    #[must_use]
    #[allow(dead_code)]
    pub fn entry_count(&self) -> usize {
        self.inner.entry_count()
    }

    /// Approximate on-disk byte total.
    #[must_use]
    #[allow(dead_code)]
    pub fn total_bytes(&self) -> u64 {
        self.inner.total_bytes()
    }

    /// Whether an entry with `hash` is stored.
    #[must_use]
    pub fn contains(&self, hash: &str) -> bool {
        self.inner.contains(hash)
    }
}
