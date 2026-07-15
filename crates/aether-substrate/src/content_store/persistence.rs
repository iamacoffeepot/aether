//! Disk persistence layer for [`super::ContentStore`]: index restore from
//! disk, sidecar read/write, root setup, and `lock.pid` acquisition.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::atomic_write::atomic_write;
use crate::pid_lock::{LockAcquisition, LockGuard, acquire_lock_pid};

use super::{Entry, SidecarRecord, TARGET};

/// The in-memory index [`restore`] rebuilds from disk.
pub struct RestoredIndex<M> {
    pub entries: HashMap<String, Entry<M>>,
    pub names: HashMap<String, String>,
    pub total_bytes: u64,
    pub clock: u64,
    pub next_seq: u64,
}

/// Rebuild the in-memory index from disk: every `entries/<hash>.manifest`
/// sidecar paired with its `<hash>` bytes, plus the `names.json` map.
pub fn restore<M: DeserializeOwned>(root: &Path) -> RestoredIndex<M> {
    let mut entries: HashMap<String, Entry<M>> = HashMap::new();
    let mut total_bytes: u64 = 0;
    let mut clock: u64 = 0;
    let mut max_uploaded_seq: u64 = 0;
    let entries_dir = root.join("entries");
    match fs::read_dir(&entries_dir) {
        Ok(read_dir) => {
            for dirent in read_dir.flatten() {
                let path = dirent.path();
                // Only sidecars drive restoration; the bytes file is keyed off it.
                if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
                    continue;
                }
                let Some(hash) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Some(sidecar) = read_sidecar::<M>(&path) else {
                    continue;
                };
                let Ok(meta) = fs::metadata(entries_dir.join(hash)) else {
                    // Sidecar without bytes — a torn write; skip it.
                    continue;
                };
                let bytes_len = meta.len();
                clock += 1;
                max_uploaded_seq = max_uploaded_seq.max(sidecar.uploaded_seq);
                entries.insert(
                    hash.to_owned(),
                    Entry {
                        metadata: sidecar.metadata,
                        bytes_len,
                        pinned: false,
                        last_access: clock,
                        uploaded_seq: sidecar.uploaded_seq,
                    },
                );
                total_bytes = total_bytes.saturating_add(bytes_len);
            }
        }
        // A missing entries dir is a fresh store — nothing to restore. Any
        // other read error (a permissions fault, say) is worth surfacing
        // rather than silently restoring an empty store.
        Err(e) if e.kind() != ErrorKind::NotFound => {
            tracing::warn!(target: TARGET, error = %e, "content store: reading entries dir failed; restoring empty");
        }
        Err(_) => {}
    }
    // Names: keep only entries that still resolve to a stored hash. A
    // missing names.json is normal (no names yet); a read or parse error
    // is logged rather than swallowed.
    let mut names: HashMap<String, String> = HashMap::new();
    match fs::read(root.join("names.json")) {
        Ok(bytes) => match serde_json::from_slice::<HashMap<String, String>>(&bytes) {
            Ok(stored) => {
                for (name, hash) in stored {
                    if entries.contains_key(&hash) {
                        names.insert(name, hash);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: TARGET, error = %e, "content store: parsing names.json failed; ignoring names");
            }
        },
        Err(e) if e.kind() != ErrorKind::NotFound => {
            tracing::warn!(target: TARGET, error = %e, "content store: reading names.json failed; ignoring names");
        }
        Err(_) => {}
    }
    RestoredIndex { entries, names, total_bytes, clock, next_seq: max_uploaded_seq.saturating_add(1) }
}

fn read_sidecar<M: DeserializeOwned>(path: &Path) -> Option<SidecarRecord<M>> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_sidecar<M: Serialize>(path: &Path, sidecar: &SidecarRecord<M>) -> io::Result<()> {
    let bytes = serde_json::to_vec(sidecar).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    atomic_write(path, &bytes)
}

/// Ensure `root/entries` exists, falling back to a unique temp dir when
/// the configured root can't be created. Returns the usable root, or the
/// fallback-creation error when even the temp dir can't be created — so a
/// total storage failure surfaces to the caller instead of yielding a
/// path to a directory that does not exist.
pub fn ensure_root(root: &Path) -> io::Result<PathBuf> {
    if fs::create_dir_all(root.join("entries")).is_ok() {
        return Ok(root.to_path_buf());
    }
    let fallback = env::temp_dir().join(format!("aether-content-store-{}-{}", process::id(), now_nanos()));
    if let Err(e) = fs::create_dir_all(fallback.join("entries")) {
        tracing::warn!(target: TARGET, error = %e, "content store: temp fallback dir creation failed");
        return Err(e);
    }
    tracing::warn!(
        target: TARGET,
        requested = %root.display(),
        fallback = %fallback.display(),
        "content store: configured root unusable; using a temp fallback",
    );
    Ok(fallback)
}

/// Best-effort `lock.pid` acquisition (ADR-0115). Delegates to
/// [`acquire_lock_pid`] for the shared
/// read → classify → reclaim → write protocol. A stale (dead-pid) or
/// garbage lock is reclaimed; a live holder or a write failure leaves
/// the store unlocked (returns `None`) but still operating, since a
/// content-addressed store tolerates a shared dir.
pub fn acquire_lock(root: &Path) -> Option<LockGuard> {
    let path = root.join("lock.pid");
    match acquire_lock_pid(&path) {
        LockAcquisition::Acquired(guard) => Some(guard),
        LockAcquisition::Held(pid) => {
            tracing::warn!(
                target: TARGET,
                path = %path.display(),
                holder_pid = pid,
                "content store: lock held by a live process; operating unlocked",
            );
            None
        }
        LockAcquisition::WriteFailed(e) => {
            tracing::warn!(
                target: TARGET,
                path = %path.display(),
                error = %e,
                "content store: writing lock.pid failed; operating unlocked",
            );
            None
        }
    }
}

/// sha256 hex over `bytes` — the content address.
pub fn hash_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Nanoseconds since the Unix epoch, for temp-dir and tmp-file nonces.
#[must_use]
pub fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}
