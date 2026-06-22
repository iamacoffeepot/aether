//! Disk persistence layer for [`super::ArtifactStore`]: index restore from
//! disk, sidecar read/write, root setup, and `lock.pid` acquisition.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use aether_substrate::atomic_write::atomic_write;
use aether_substrate::pid_lock::{LockAcquisition, LockGuard, acquire_lock_pid};

use super::{Entry, StoredEntry, TARGET};

/// The in-memory index [`restore`] rebuilds from disk.
pub(super) struct RestoredIndex {
    pub(super) entries: HashMap<String, Entry>,
    pub(super) names: HashMap<String, String>,
    pub(super) total_bytes: u64,
    pub(super) clock: u64,
}

/// Rebuild the in-memory index from disk: every `entries/<hash>.manifest`
/// sidecar paired with its `<hash>` bytes, plus the `names.json` map.
pub(super) fn restore(root: &Path) -> RestoredIndex {
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

pub(super) fn write_sidecar(path: &Path, sidecar: &StoredEntry) -> io::Result<()> {
    let bytes =
        serde_json::to_vec(sidecar).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    atomic_write(path, &bytes)
}

/// Ensure `root/entries` exists, falling back to a unique temp dir when
/// the configured root can't be created — so the store always opens.
pub(super) fn ensure_root(root: &Path) -> PathBuf {
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

/// Best-effort `lock.pid` acquisition (ADR-0115). Delegates to
/// [`aether_substrate::pid_lock::acquire_lock_pid`] for the shared
/// read → classify → reclaim → write protocol. A stale (dead-pid) or
/// garbage lock is reclaimed; a live holder or a write failure leaves
/// the store unlocked (returns `None`) but still operating, since a
/// content-addressed store tolerates a shared dir.
pub(super) fn acquire_lock(root: &Path) -> Option<LockGuard> {
    let path = root.join("lock.pid");
    match acquire_lock_pid(&path) {
        LockAcquisition::Acquired(guard) => Some(guard),
        LockAcquisition::Held(pid) => {
            tracing::warn!(
                target: TARGET,
                path = %path.display(),
                holder_pid = pid,
                "binary store: lock held by a live process; operating unlocked",
            );
            None
        }
        LockAcquisition::WriteFailed(e) => {
            tracing::warn!(
                target: TARGET,
                path = %path.display(),
                error = %e,
                "binary store: writing lock.pid failed; operating unlocked",
            );
            None
        }
    }
}

/// sha256 hex over `bytes` — the content address.
pub(super) fn hash_hex(bytes: &[u8]) -> String {
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
pub(super) fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}
