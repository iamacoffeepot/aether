//! Content-addressed artifact store for the hub (ADR-0115, issue 1953).
//!
//! The storage half of the hub artifact registry: somewhere above the
//! engine layer to keep uploaded binaries content-addressed, ingest one
//! from a staged host path, and read what each binary *is*. The per-engine
//! ADR-0049 handle store sits *below* an engine and is keyed on an assigned
//! `HandleId`; this store is hub-scoped and keyed on a sha256 over the raw
//! bytes, so an identical re-upload dedups to the same entry.
//!
//! Artifact-generic from the start — an entry is a content blob plus a
//! type-tagged ([`ArtifactKind`]) manifest — so the registry extraction
//! (#1955) lifts it whole. Today the only artifact is a chassis binary and
//! the manifest is a [`BinaryManifest`].
//!
//! ## Layout
//!
//! Under a hub-scoped, layout-versioned root
//! (`AETHER_BINARY_STORE_DIR`, default `data_dir/aether/binaries/v1`):
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
//! Modeled on the ADR-0049 handle store's lock / reclaim / budget shape
//! (`aether_substrate::handle_store`), not its instance: the handle store
//! is keyed on `HandleId` and lives per-engine. The shared `is_pid_alive`
//! liveness check is the one piece literally reused. The `aether.engine`
//! cap is single-threaded (one dispatcher run-token), so the store holds
//! its index in plain fields behind `&mut self` rather than an inner lock.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use aether_kinds::{BinaryEntry, BinaryManifest, ListBinaries};
use aether_substrate::handle_store::is_pid_alive;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Env override for the store's layout root (the ops escape hatch and the
/// per-process isolation knob the fleet tests set). Absent → the platform
/// data dir, then a temp fallback.
pub const ENV_BINARY_STORE_DIR: &str = "AETHER_BINARY_STORE_DIR";

/// Layout-version subdirectory under the resolved root, so a future
/// on-disk format change can land beside `v1` without a migration.
pub const LAYOUT_VERSION_DIR: &str = "v1";

/// Default on-disk byte budget. 16 GiB — matches the handle store's disk
/// budget; binaries are tens of megabytes, so this holds a deep history
/// before LRU eviction kicks in.
pub const DEFAULT_DISK_BUDGET_BYTES: u64 = 16 * 1024 * 1024 * 1024;

const TARGET: &str = "aether_capabilities::store";

/// The type tag on a stored artifact (ADR-0115). One variant today — the
/// store is artifact-generic so #1955 can add more (component packs, asset
/// bundles) without reshaping the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    /// A chassis substrate binary, described by a [`BinaryManifest`].
    Binary,
}

/// How a caller addresses a stored artifact in [`ArtifactStore::get`] —
/// by its content hash or by a human-readable name. The seam #1954's
/// spawn cutover consumes to resolve a registry reference to bytes.
#[derive(Debug, Clone)]
pub enum Selector {
    /// The sha256 hex content address.
    Hash(String),
    /// A name an upload pointed at a hash.
    Name(String),
}

/// One resolved artifact returned by [`ArtifactStore::get`]: its content
/// hash, the on-disk path of its raw bytes (the fork target for #1954),
/// the type tag, the manifest, and the name pointing at it (if any).
#[derive(Debug, Clone)]
pub struct StoredArtifact {
    pub hash: String,
    pub path: PathBuf,
    pub kind: ArtifactKind,
    pub manifest: BinaryManifest,
    pub name: Option<String>,
}

/// The JSON sidecar written next to each entry's bytes — the type tag
/// plus the manifest, so a fresh store rebuilds its index from disk.
#[derive(Serialize, Deserialize, Clone)]
struct StoredEntry {
    kind: ArtifactKind,
    manifest: BinaryManifest,
}

/// In-memory record of one entry. The bytes live on disk at
/// `entries/<hash>`; only the metadata is held in memory (binaries are
/// large), read back lazily on [`ArtifactStore::get`].
struct Entry {
    kind: ArtifactKind,
    manifest: BinaryManifest,
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
    /// Resolve the layout root from the environment, then [`open`] it.
    ///
    /// Priority: `AETHER_BINARY_STORE_DIR` env override, then
    /// `data_dir/aether/binaries`, then `temp_dir/aether-binaries`. The
    /// [`LAYOUT_VERSION_DIR`] is always appended.
    ///
    /// [`open`]: Self::open
    #[must_use]
    pub fn from_env() -> Self {
        Self::open(&root_from_env(), DEFAULT_DISK_BUDGET_BYTES)
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
        manifest: BinaryManifest,
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
    /// constraint".
    #[must_use]
    pub fn list(&self, filter: &ListBinaries) -> Vec<BinaryEntry> {
        self.entries
            .iter()
            .filter(|(_, entry)| matches_filter(&entry.manifest, filter))
            .map(|(hash, entry)| BinaryEntry {
                hash: hash.clone(),
                name: self.name_for(hash),
                manifest: entry.manifest.clone(),
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

    /// Evict LRU entries that are neither pinned nor named until the disk
    /// ledger is back under budget (or no eligible candidate remains).
    fn evict_if_needed(&mut self) {
        while self.total_bytes > self.disk_budget_bytes {
            // Snapshot the named set once (a name protects its target), then
            // pick the oldest eligible entry. Both reads borrow disjoint
            // fields; `victim` is owned, so the removal below is clear.
            let named: HashSet<&str> = self.names.values().map(String::as_str).collect();
            let victim = self
                .entries
                .iter()
                .filter(|(hash, entry)| !entry.pinned && !named.contains(hash.as_str()))
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(hash, _)| hash.clone());
            drop(named);
            let Some(hash) = victim else {
                // Everything left is protected; can't shrink further.
                break;
            };
            if let Some(entry) = self.entries.remove(&hash) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.bytes_len);
                let (bytes_path, manifest_path) = self.entry_paths(&hash);
                let _ = fs::remove_file(&bytes_path);
                let _ = fs::remove_file(&manifest_path);
                tracing::info!(target: TARGET, hash = %hash, "binary store: evicted to hold disk budget");
            }
        }
    }
}

/// Whether a manifest passes a [`ListBinaries`] filter.
fn matches_filter(manifest: &BinaryManifest, filter: &ListBinaries) -> bool {
    if let Some(chassis) = &filter.chassis
        && &manifest.chassis != chassis
    {
        return false;
    }
    if let Some(target) = &filter.target
        && &manifest.target != target
    {
        return false;
    }
    filter.caps.iter().all(|c| manifest.caps.contains(c))
}

/// sha256 hex over `bytes` — the content address.
fn hash_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Resolve the layout root from the environment (see
/// [`ArtifactStore::from_env`]).
fn root_from_env() -> PathBuf {
    if let Ok(raw) = env::var(ENV_BINARY_STORE_DIR)
        && !raw.is_empty()
    {
        return PathBuf::from(raw).join(LAYOUT_VERSION_DIR);
    }
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

/// Ensure `root/entries` exists, falling back to a unique temp dir when
/// the configured root can't be created — so the store always opens.
fn ensure_root(root: &Path) -> PathBuf {
    if fs::create_dir_all(root.join("entries")).is_ok() {
        return root.to_path_buf();
    }
    let fallback =
        env::temp_dir().join(format!("aether-binaries-{}-{}", process::id(), now_nanos()));
    if let Err(e) = fs::create_dir_all(fallback.join("entries")) {
        tracing::warn!(target: TARGET, error = %e, "binary store: temp fallback dir creation failed");
    } else {
        tracing::warn!(
            target: TARGET,
            requested = %root.display(),
            fallback = %fallback.display(),
            "binary store: configured root unusable; using a temp fallback",
        );
    }
    fallback
}

/// Best-effort `lock.pid` acquisition (ADR-0115; reuses the handle
/// store's reclaim shape). A stale (dead-pid) or garbage lock is
/// reclaimed and rewritten with our pid; a live holder leaves the store
/// unlocked (returns `None`) but still operating, since a content-
/// addressed store tolerates a shared dir.
fn acquire_lock(root: &Path) -> Option<LockGuard> {
    let path = root.join("lock.pid");
    if let Ok(raw) = fs::read_to_string(&path) {
        match raw.trim().parse::<i32>() {
            Ok(pid) if pid > 0 && is_pid_alive(pid) => {
                // A live holder (possibly this same process re-opening the
                // dir) leaves us unlocked but still operating.
                tracing::warn!(
                    target: TARGET,
                    path = %path.display(),
                    holder_pid = pid,
                    "binary store: lock held by a live process; operating unlocked",
                );
                return None;
            }
            Ok(_) => {}
            Err(_) => {
                tracing::warn!(target: TARGET, path = %path.display(), "binary store: lock.pid holds garbage; reclaiming");
            }
        }
    }
    if let Err(e) = atomic_write(&path, process::id().to_string().as_bytes()) {
        tracing::warn!(target: TARGET, path = %path.display(), error = %e, "binary store: writing lock.pid failed; operating unlocked");
        return None;
    }
    Some(LockGuard { path })
}

/// The in-memory index [`restore`] rebuilds from disk.
struct RestoredIndex {
    entries: HashMap<String, Entry>,
    names: HashMap<String, String>,
    total_bytes: u64,
    clock: u64,
}

/// Rebuild the in-memory index from disk: every `entries/<hash>.manifest`
/// sidecar paired with its `<hash>` bytes, plus the `names.json` map.
fn restore(root: &Path) -> RestoredIndex {
    let mut entries: HashMap<String, Entry> = HashMap::new();
    let mut total_bytes: u64 = 0;
    let mut clock: u64 = 0;
    let entries_dir = root.join("entries");
    if let Ok(read_dir) = fs::read_dir(&entries_dir) {
        for dirent in read_dir.flatten() {
            let path = dirent.path();
            // Only sidecars drive restoration; the bytes file is keyed off it.
            if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
                continue;
            }
            let Some(hash) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(sidecar) = read_sidecar(&path) else {
                continue;
            };
            let Ok(meta) = fs::metadata(entries_dir.join(hash)) else {
                // Sidecar without bytes — a torn write; skip it.
                continue;
            };
            let bytes_len = meta.len();
            clock += 1;
            entries.insert(
                hash.to_owned(),
                Entry {
                    kind: sidecar.kind,
                    manifest: sidecar.manifest,
                    bytes_len,
                    pinned: false,
                    last_access: clock,
                },
            );
            total_bytes = total_bytes.saturating_add(bytes_len);
        }
    }
    // Names: keep only entries that still resolve to a stored hash.
    let mut names: HashMap<String, String> = HashMap::new();
    if let Ok(bytes) = fs::read(root.join("names.json"))
        && let Ok(stored) = serde_json::from_slice::<HashMap<String, String>>(&bytes)
    {
        for (name, hash) in stored {
            if entries.contains_key(&hash) {
                names.insert(name, hash);
            }
        }
    }
    RestoredIndex {
        entries,
        names,
        total_bytes,
        clock,
    }
}

fn read_sidecar(path: &Path) -> Option<StoredEntry> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_sidecar(path: &Path, sidecar: &StoredEntry) -> io::Result<()> {
    let bytes =
        serde_json::to_vec(sidecar).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    atomic_write(path, &bytes)
}

/// Nanoseconds since the Unix epoch, for temp-dir and tmp-file nonces.
fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Atomic write via tmp + rename (the handle store's pattern): stage to a
/// sibling `.tmp-<pid>-<nonce>`, fsync, rename over the target. Creates
/// the parent dir lazily.
fn atomic_write(target: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("entry");
    let tmp = target.with_file_name(format!("{file_name}.tmp-{}-{}", process::id(), now_nanos()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// RAII guard that deletes `lock.pid` on graceful shutdown. SIGKILL
/// bypasses `Drop`; the stale-lock reclaim on the next open handles that.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::{ArtifactKind, ArtifactStore, DEFAULT_DISK_BUDGET_BYTES, ListBinaries, Selector};
    use std::path::PathBuf;
    use std::{env, fs, process};

    fn temp_root(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "aether-binstore-test-{label}-{}-{}",
            process::id(),
            super::now_nanos()
        ))
    }

    fn manifest(chassis: &str) -> aether_kinds::BinaryManifest {
        aether_kinds::BinaryManifest {
            chassis: chassis.to_owned(),
            caps: vec!["aether.fs".to_owned()],
            git_sha: "deadbee".to_owned(),
            profile: "debug".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
        }
    }

    #[test]
    fn upload_dedups_identical_bytes_to_one_hash() {
        let root = temp_root("dedup");
        let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
        let h1 = store.upload(
            b"the-binary-bytes",
            ArtifactKind::Binary,
            manifest("headless"),
            None,
        );
        let h2 = store.upload(
            b"the-binary-bytes",
            ArtifactKind::Binary,
            manifest("headless"),
            None,
        );
        assert_eq!(h1, h2, "identical bytes dedup to the same content hash");
        assert_eq!(store.entry_count(), 1, "dedup stores one entry");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn name_repoints_to_the_latest_uploaded_hash() {
        let root = temp_root("repoint");
        let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
        let h_old = store.upload(
            b"v1",
            ArtifactKind::Binary,
            manifest("headless"),
            Some("engine".to_owned()),
        );
        let h_new = store.upload(
            b"v2",
            ArtifactKind::Binary,
            manifest("headless"),
            Some("engine".to_owned()),
        );
        assert_ne!(h_old, h_new);
        let resolved = store
            .get(&Selector::Name("engine".to_owned()))
            .expect("the name resolves");
        assert_eq!(resolved.hash, h_new, "the name points at the latest upload");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn entries_persist_across_a_reopen() {
        let root = temp_root("persist");
        let hash = {
            let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
            store.upload(
                b"persisted-bytes",
                ArtifactKind::Binary,
                manifest("headless"),
                Some("svc".to_owned()),
            )
            // store drops here — LockGuard releases lock.pid
        };
        let mut reopened = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
        assert!(reopened.contains(&hash), "the entry survives a reopen");
        let resolved = reopened
            .get(&Selector::Name("svc".to_owned()))
            .expect("the name survives a reopen");
        assert_eq!(resolved.hash, hash);
        let bytes = fs::read(&resolved.path).expect("the stored bytes are readable");
        assert_eq!(bytes, b"persisted-bytes");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn eviction_skips_pinned_and_named_entries() {
        let root = temp_root("evict");
        // Budget holds the three ~10-byte initial entries (≈31 bytes) but
        // not a fourth, so the trigger upload forces exactly one eviction —
        // of the only unnamed, unpinned candidate.
        let mut store = ArtifactStore::open(&root, 40);
        // Unnamed, unpinned — the eviction candidate.
        let h_plain = store.upload(
            b"plain-aaaa",
            ArtifactKind::Binary,
            manifest("headless"),
            None,
        );
        // Named — protected.
        let h_named = store.upload(
            b"named-bbbb",
            ArtifactKind::Binary,
            manifest("headless"),
            Some("keep".to_owned()),
        );
        // Pinned — protected.
        let h_pinned = store.upload(
            b"pinned-cccc",
            ArtifactKind::Binary,
            manifest("headless"),
            None,
        );
        assert!(store.pin(&h_pinned), "pin targets a stored entry");
        // Force eviction by re-running it through another over-budget upload.
        let _ = store.upload(
            b"trigger-dddd",
            ArtifactKind::Binary,
            manifest("headless"),
            None,
        );

        assert!(store.contains(&h_named), "a named entry is never evicted");
        assert!(store.contains(&h_pinned), "a pinned entry is never evicted");
        assert!(
            !store.contains(&h_plain),
            "the oldest unnamed, unpinned entry is evicted first",
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_applies_chassis_and_caps_filters() {
        let root = temp_root("filter");
        let mut store = ArtifactStore::open(&root, DEFAULT_DISK_BUDGET_BYTES);
        store.upload(
            b"headless-bin",
            ArtifactKind::Binary,
            manifest("headless"),
            None,
        );
        let mut desktop = manifest("desktop");
        desktop.caps = vec!["aether.fs".to_owned(), "aether.render".to_owned()];
        store.upload(b"desktop-bin", ArtifactKind::Binary, desktop, None);

        let headless_only = store.list(&ListBinaries {
            chassis: Some("headless".to_owned()),
            caps: vec![],
            target: None,
        });
        assert_eq!(headless_only.len(), 1);
        assert_eq!(headless_only[0].manifest.chassis, "headless");

        let render_capable = store.list(&ListBinaries {
            chassis: None,
            caps: vec!["aether.render".to_owned()],
            target: None,
        });
        assert_eq!(
            render_capable.len(),
            1,
            "only the desktop binary links render",
        );
        assert_eq!(render_capable[0].manifest.chassis, "desktop");

        let all = store.list(&ListBinaries::default());
        assert_eq!(all.len(), 2);
        let _ = fs::remove_dir_all(&root);
    }
}
