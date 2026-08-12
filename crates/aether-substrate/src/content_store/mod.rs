//! Content-addressed, disk-backed artifact storage core (ADR-0115,
//! ADR-0149). A domain-neutral primitive: sha256 content addressing, an
//! on-disk entry index, atomic sidecar persistence + restore, `lock.pid`
//! acquisition, and an eviction step behind a policy. It owns no artifact
//! vocabulary — the per-entry metadata is a caller-supplied type `M`, and
//! the store root is a caller-supplied path.
//!
//! Two consumers share it: the hub's `ArtifactStore` (binary / component
//! artifacts, LRU-budget eviction) and Bloomery's eviction-free
//! `artifacts` canonical record (ADR-0149). Both live in higher crates
//! (`aether-fleet`, `aether-chassis-bloomery`), so the core sits beside the
//! two primitives it builds on (`atomic_write`, `pid_lock`) and closes no
//! crate cycle.
//!
//! ## Layout
//!
//! Under a caller-supplied root:
//!
//! ```text
//! <root>/
//!   entries/
//!     <hash>            the raw bytes (content-addressed)
//!     <hash>.manifest   the sidecar metadata, JSON
//!   names.json          name -> hash map
//!   lock.pid            owning-process pid (best-effort reclaim)
//! ```
//!
//! The store is single-owner: it holds its index in plain fields behind
//! `&mut self` rather than an inner lock, matching the single-threaded
//! `aether.fleet` cap that first hosted this core.
//!
//! ## The index is a cache, the directory is the truth
//!
//! One root can carry several live handles — the bloomery chassis mounts
//! its artifacts capability and its executor reactor opens a second
//! handle on the same root, and a second *process* can do the same. Each
//! handle builds its index once at open, so the index is a cache over the
//! content-addressed directory rather than the store's only truth: a
//! [`get`](ContentStore::get) miss and an [`upload`](ContentStore::upload)
//! consult disk before answering, and [`refresh`](ContentStore::refresh)
//! is the enumeration-side counterpart. Without that, a handle is blind
//! to every entry a peer wrote after it opened, and its `upload` of bytes
//! a peer already stored would overwrite the peer's sidecar metadata.

mod eviction;
mod persistence;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::atomic_write::atomic_write;
use crate::pid_lock::LockGuard;

pub use persistence::now_nanos;
use persistence::{
    DiskEntry, RestoredIndex, acquire_lock, ensure_root, hash_hex, read_entry, read_names, restore, write_sidecar,
};

const TARGET: &str = "aether_substrate::content_store";

/// How a [`ContentStore`] reclaims disk when an upload lands.
///
/// [`LruBudget`](EvictionPolicy::LruBudget) evicts the
/// least-recently-used entries that are neither pinned nor named until the
/// on-disk ledger is back under the byte budget — cache semantics correct
/// for re-uploadable artifacts. [`None`](EvictionPolicy::None) never
/// evicts — a canonical record that must retain every entry (ADR-0149),
/// where the eviction step is a cheap early return.
#[derive(Debug, Clone, Copy)]
pub enum EvictionPolicy {
    /// Evict LRU unpinned, unnamed entries to hold this on-disk byte
    /// budget.
    LruBudget(u64),
    /// Never evict — retain every entry regardless of recency or budget.
    None,
}

/// How a caller addresses a stored entry in [`ContentStore::get`] — by its
/// content hash or by a name an upload pointed at a hash.
#[derive(Debug, Clone)]
pub enum Selector {
    /// The sha256 hex content address.
    Hash(String),
    /// A name an upload pointed at a hash.
    Name(String),
}

/// One entry resolved by [`ContentStore::get`]: its content hash, the
/// on-disk path of its raw bytes, a clone of its metadata, and the name
/// pointing at it (if any). Consumers wrap this in their own resolved type.
#[derive(Debug, Clone)]
pub struct Resolved<M> {
    /// The sha256 hex content address.
    pub hash: String,
    /// On-disk path of the raw bytes.
    pub path: PathBuf,
    /// The per-entry metadata this store holds for the entry.
    pub metadata: M,
    /// The lexicographically smallest name pointing at the entry, if any.
    pub name: Option<String>,
}

/// A borrowed view of one indexed entry, yielded by
/// [`ContentStore::entries`] for consumer-side projections that need the
/// hash, the metadata, and the ingest sequence without reaching private
/// fields.
pub struct EntryRef<'a, M> {
    /// The sha256 hex content address.
    pub hash: &'a str,
    /// The per-entry metadata.
    pub metadata: &'a M,
    /// Stable first-ingest sequence (newest-first listing key).
    pub uploaded_seq: u64,
}

/// The JSON sidecar written next to each entry's bytes: the caller's
/// metadata flattened alongside the ingest sequence. Flattening keeps the
/// metadata's own fields at the object's top level, so a consumer whose
/// `M` is `{ a, b }` sidecars `{ a, b, uploaded_seq }` — no wrapper key.
#[derive(Serialize, serde::Deserialize)]
struct SidecarRecord<M> {
    #[serde(flatten)]
    metadata: M,
    #[serde(default)]
    uploaded_seq: u64,
}

/// In-memory record of one entry. The bytes live on disk at
/// `entries/<hash>`; only the metadata is held in memory (artifacts are
/// large), read back lazily on [`ContentStore::get`].
struct Entry<M> {
    metadata: M,
    bytes_len: u64,
    /// Eviction protection independent of naming (an explicit
    /// [`ContentStore::pin`]). A named entry is also eviction-protected.
    pinned: bool,
    /// Monotonic access stamp; lower = older, the LRU eviction key.
    last_access: u64,
    /// Stable first-ingest sequence. Reads and deduplicated uploads never
    /// change it; listing pages sort newest-first by this value.
    uploaded_seq: u64,
}

/// Content-addressed, disk-backed artifact store parameterized over the
/// per-entry metadata type `M` and an [`EvictionPolicy`] (ADR-0115,
/// ADR-0149). Owned single-threaded, so its index lives in plain fields
/// behind `&mut self`.
pub struct ContentStore<M> {
    /// The root holding `entries/`, `names.json`, `lock.pid`.
    root: PathBuf,
    policy: EvictionPolicy,
    /// hash -> entry metadata.
    entries: HashMap<String, Entry<M>>,
    /// name -> hash. Repointing a name to a new hash is a plain overwrite;
    /// the old hash keeps its bytes but loses its name (and so its
    /// eviction protection).
    names: HashMap<String, String>,
    /// Approximate on-disk byte ledger, the LRU eviction trigger.
    total_bytes: u64,
    /// Monotonic source for `Entry::last_access`.
    clock: u64,
    /// Sequence to assign to the next successfully persisted new content
    /// hash. It advances only after both bytes and sidecar are durable.
    next_seq: u64,
    /// `lock.pid` guard. Held for the store's lifetime when the lock was
    /// freshly written; `None` when another live process holds it (the
    /// store still operates — a content-addressed store tolerates a shared
    /// dir, so the lock is hygiene, not a hard mutex). What a shared root
    /// costs is staleness rather than corruption: this handle's index is
    /// built at open, and a peer's later writes reach it through the
    /// miss-path adopt in [`get`](ContentStore::get) /
    /// [`upload`](ContentStore::upload) or an explicit
    /// [`refresh`](ContentStore::refresh).
    _lock: Option<LockGuard>,
}

impl<M: Serialize + DeserializeOwned + Clone> ContentStore<M> {
    /// Open (or create) the store at `root` with the given eviction
    /// policy. A root that can't be created falls back to a unique temp
    /// dir, so a caller normally comes up with a working store; only a
    /// total storage failure — the configured root *and* the temp
    /// fallback both uncreatable — returns an error rather than a store
    /// pointed at a directory that does not exist. The `lock.pid` reclaim
    /// is best-effort — a stale (dead-pid) or garbage lock is reclaimed;
    /// a live holder leaves the store unlocked but still operating.
    pub fn open(root: &Path, policy: EvictionPolicy) -> io::Result<Self> {
        let root = ensure_root(root)?;
        let lock = acquire_lock(&root);
        let RestoredIndex { entries, names, total_bytes, clock, next_seq } = restore(&root);
        Ok(Self { root, policy, entries, names, total_bytes, clock, next_seq, _lock: lock })
    }

    /// The root this store resolved to (after any temp fallback).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ingest `bytes` content-addressed, recording `metadata` and
    /// (optionally) pointing `name` at the resulting hash. A re-upload of
    /// identical bytes dedups: the bytes aren't rewritten and the same
    /// hash comes back, but a fresh `name` still repoints. Returns the
    /// sha256 hex the bytes stored under. Runs the eviction policy
    /// afterward.
    ///
    /// Dedup is decided against the directory, not only this handle's
    /// index: bytes a peer handle on the same root already stored are
    /// adopted first, so they dedup rather than being rewritten under
    /// this caller's `metadata`. That distinction is the difference
    /// between keeping and losing the peer's sidecar — bloomery records
    /// an artifact's derivation parents there.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] when new content can't be
    /// persisted — the bytes or the sidecar failed to write. A returned
    /// error means nothing was indexed and no name was repointed, so the
    /// caller must not treat the upload as stored: there is no hash to
    /// hand out, and a later [`get`](Self::get) of one would miss.
    pub fn upload(&mut self, bytes: &[u8], metadata: M, name: Option<String>) -> io::Result<String> {
        let hash = hash_hex(bytes);
        self.adopt_from_disk(&hash);
        let clock = self.next_clock();

        let stored = match self.entries.get_mut(&hash) {
            // Dedup: bump recency so a re-uploaded entry isn't the first
            // eviction target.
            Some(entry) => {
                entry.last_access = clock;
                Ok(())
            }
            None => self.persist_new(&hash, bytes, metadata, clock),
        };

        if stored.is_ok()
            && let Some(name) = name
        {
            self.names.insert(name, hash.clone());
            self.persist_names();
        }

        // Run the policy either way: a failed write is exactly when disk
        // pressure is most likely the cause, so reclaiming is worthwhile.
        self.evict_if_needed();
        stored.map(|()| hash)
    }

    /// Write new content's bytes + sidecar and index it. A failure indexes
    /// nothing and leaves no partial entry on disk, so the store stays
    /// consistent — the next upload of the same bytes retries.
    fn persist_new(&mut self, hash: &str, bytes: &[u8], metadata: M, clock: u64) -> io::Result<()> {
        let (bytes_path, manifest_path) = self.entry_paths(hash);
        let uploaded_seq = self.next_seq;
        let sidecar = SidecarRecord { metadata: metadata.clone(), uploaded_seq };

        atomic_write(&bytes_path, bytes)?;
        if let Err(e) = write_sidecar(&manifest_path, &sidecar) {
            // The cleanup failure is swallowed — it has no channel of its
            // own, and the sidecar error is the one the caller needs.
            if let Err(cleanup_e) = fs::remove_file(&bytes_path) {
                tracing::warn!(target: TARGET, hash = %hash, error = %cleanup_e, "content store: cleaning up orphaned entry bytes after failed sidecar write");
            }
            return Err(e);
        }

        let bytes_len = bytes.len() as u64;
        self.entries
            .insert(hash.to_owned(), Entry { metadata, bytes_len, pinned: false, last_access: clock, uploaded_seq });
        self.total_bytes = self.total_bytes.saturating_add(bytes_len);
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(())
    }

    /// Pin (or unpin) an entry by hash, protecting it from eviction
    /// independent of whether a name points at it. Returns `false` if no
    /// entry has that hash. Persistence of the pin flag is a fast-follow —
    /// today a pin holds for the store's lifetime.
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

    /// Index an entry that is on disk but not in this handle's index —
    /// the miss path that makes the index a cache over the directory
    /// rather than the store's only truth. Returns whether it adopted
    /// one: `false` when the hash is already indexed, or when no complete
    /// entry (sidecar plus bytes) exists under the root.
    ///
    /// The adopted entry starts unpinned with a fresh recency stamp —
    /// pins and recency are per-handle in-memory state that no peer
    /// writes to disk — while its `uploaded_seq` comes from the sidecar,
    /// so the peer's ingest order survives and this handle's `next_seq`
    /// advances past it rather than reissuing a sequence already spent.
    ///
    /// Adopting takes the name map with it. The same peer `upload` that
    /// wrote the entry may have pointed a name at it, and that name is
    /// part of what the entry *is* — [`Resolved::name`] and
    /// [`name_for`](Self::name_for) would otherwise answer `None` for a
    /// named entry reached by hash while answering `Some` for the same
    /// entry reached by name, which is a wrong answer rather than a stale
    /// one. A name also protects its target from eviction, so leaving it
    /// behind would under-protect an entry the root says is named.
    fn adopt_from_disk(&mut self, hash: &str) -> bool {
        if self.entries.contains_key(hash) {
            return false;
        }
        let Some(DiskEntry { metadata, uploaded_seq, bytes_len }) = read_entry::<M>(&self.root, hash) else {
            return false;
        };

        let last_access = self.next_clock();
        self.entries.insert(hash.to_owned(), Entry { metadata, bytes_len, pinned: false, last_access, uploaded_seq });
        self.total_bytes = self.total_bytes.saturating_add(bytes_len);
        self.next_seq = self.next_seq.max(uploaded_seq.saturating_add(1));
        self.merge_names_from_disk();
        true
    }

    /// Take the `names.json` mappings this handle lacks. Only missing
    /// mappings are taken; one this handle already holds wins.
    fn merge_names_from_disk(&mut self) {
        for (name, hash) in read_names(&self.root) {
            self.names.entry(name).or_insert(hash);
        }
    }

    /// Resolve a name to the hash it points at, re-reading `names.json`
    /// once on a miss so a name a peer handle pointed after this handle
    /// opened still resolves.
    ///
    /// Only missing mappings are taken; one this handle already holds
    /// wins. Naming under two writers is not made coherent here — each
    /// writer rewrites the whole file from its own map, so the last
    /// `upload` decides what a shared `names.json` contains either way.
    /// This resolves the miss that made a peer's name unreachable; it
    /// does not turn the name map into shared state. Both consumers on a
    /// shared root (bloomery's artifacts record) upload unnamed.
    fn resolve_name(&mut self, name: &str) -> Option<String> {
        if let Some(hash) = self.names.get(name) {
            return Some(hash.clone());
        }
        self.merge_names_from_disk();
        self.names.get(name).cloned()
    }

    /// Resolve an entry by hash or name to its on-disk path + a clone of
    /// its metadata. `None` if the hash / name isn't stored. Bumps the
    /// entry's recency.
    ///
    /// A miss in this handle's index is not the answer: the hash is
    /// looked up on disk and adopted if a peer handle stored it after
    /// this handle opened, and a name miss re-reads `names.json` first.
    /// This is the resolving path that [`contains`](Self::contains)
    /// deliberately is not.
    pub fn get(&mut self, selector: &Selector) -> Option<Resolved<M>> {
        let hash = match selector {
            Selector::Hash(h) => h.clone(),
            Selector::Name(n) => self.resolve_name(n)?,
        };
        self.adopt_from_disk(&hash);
        let clock = self.next_clock();
        let entry = self.entries.get_mut(&hash)?;
        entry.last_access = clock;
        let metadata = entry.metadata.clone();
        let name = self.name_for(&hash);
        let (path, _) = self.entry_paths(&hash);
        Some(Resolved { hash, path, metadata, name })
    }

    /// Adopt every entry and name under the root that this handle does
    /// not already hold — the enumeration-side counterpart of the
    /// miss-path adopt in [`get`](Self::get). [`entries`](Self::entries)
    /// and [`entry_count`](Self::entry_count) answer from the index
    /// alone, so a handle that shares its root with another live handle
    /// (or another process) calls this before it treats its own
    /// enumeration as the whole record.
    ///
    /// Entries this handle already holds are left exactly as they are:
    /// their `pinned` flag and recency stamp are in-memory state no peer
    /// can see on disk, and re-restoring over them would discard both.
    /// No eviction runs — adopting is not an ingest, and a handle asking
    /// what is on the root should not reclaim from it.
    pub fn refresh(&mut self) {
        let RestoredIndex { entries, names, .. } = restore::<M>(&self.root);

        for (hash, entry) in entries {
            if self.entries.contains_key(&hash) {
                continue;
            }
            self.total_bytes = self.total_bytes.saturating_add(entry.bytes_len);
            self.next_seq = self.next_seq.max(entry.uploaded_seq.saturating_add(1));
            let last_access = self.next_clock();
            self.entries.insert(hash, Entry { last_access, ..entry });
        }

        for (name, hash) in names {
            self.names.entry(name).or_insert(hash);
        }
    }

    /// A borrowing iterator over every indexed entry, for consumer-side
    /// projections (list / filter). Order is unspecified — the caller
    /// sorts.
    pub fn entries(&self) -> impl Iterator<Item = EntryRef<'_, M>> {
        self.entries.iter().map(|(hash, entry)| EntryRef {
            hash: hash.as_str(),
            metadata: &entry.metadata,
            uploaded_seq: entry.uploaded_seq,
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

    /// Whether an entry with `hash` is indexed *by this handle*.
    ///
    /// An index-only predicate, deliberately taken by `&self`: it never
    /// consults disk, so it answers `false` for an entry another live
    /// handle on the same root wrote after this one opened.
    /// [`get`](Self::get) is the resolving path — it adopts such an entry
    /// on a miss — and [`refresh`](Self::refresh) is the enumeration-side
    /// counterpart. The `&self` borrow is the point: the hub's selector
    /// disambiguation (`aether-fleet`) uses this as a cheap predicate
    /// over a store it is the only handle on, where the index and the
    /// directory cannot disagree.
    #[must_use]
    pub fn contains(&self, hash: &str) -> bool {
        self.entries.contains_key(hash)
    }

    /// The lexicographically smallest name pointing at `hash`.
    #[must_use]
    pub fn name_for(&self, hash: &str) -> Option<String> {
        self.names.iter().filter(|(_, h)| h.as_str() == hash).map(|(n, _)| n).min().cloned()
    }

    fn next_clock(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The `(bytes, metadata-sidecar)` paths for `hash` under `entries/`.
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
                    tracing::warn!(target: TARGET, error = %e, "content store: persisting names failed");
                }
            }
            Err(e) => {
                tracing::warn!(target: TARGET, error = %e, "content store: encoding names failed");
            }
        }
    }
}

#[cfg(test)]
mod tests;
