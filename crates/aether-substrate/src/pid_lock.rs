//! Shared `lock.pid` acquisition protocol (ADR-0049 §7 / ADR-0115).
//!
//! The `lock.pid` file format — write the owning-process pid, reclaim a
//! stale or garbage lock, delete the file on graceful shutdown — is the
//! same between the ADR-0049 handle store and the ADR-0115 hub binary
//! store. This module is the single definition of that protocol, consumed
//! by both stores; each store maps the [`LockAcquisition`] result to its
//! own divergent live-holder policy.

use std::fs;
use std::fs::OpenOptions;
use std::io::Error as IoError;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether `pid` names a live process. Unix: `kill(pid, 0)` returns 0
/// for a live process, `ESRCH` for a dead one, `EPERM` for a live one
/// we can't signal (still counts as alive). Non-Unix: conservatively
/// reports `false` so the lock is always reclaimable (substrate on
/// Windows is deferred per ADR-0049 §7).
#[cfg(unix)]
#[must_use]
pub fn is_pid_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 performs the error checks without
    // sending a signal. No memory is touched.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // errno == EPERM means the process exists but we lack permission.
    IoError::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
#[must_use]
pub fn is_pid_alive(_pid: i32) -> bool {
    false
}

/// RAII guard that deletes `lock.pid` on graceful shutdown. SIGKILL
/// bypasses `Drop`; the stale-lock reclamation path handles that case
/// on the next open.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Outcome of [`acquire_lock_pid`].
pub enum LockAcquisition {
    /// Any stale or garbage lock was reclaimed; our pid has been written.
    /// The guard deletes `lock.pid` on drop.
    Acquired(LockGuard),
    /// A live process holds the lock. The caller decides whether to abort
    /// or operate without the lock.
    Held(i32),
    /// The pid write itself failed. The caller decides how to handle it.
    WriteFailed(IoError),
}

/// Bound on the reclaim-and-retry loop: each pass reclaims one stale lock
/// and re-attempts the atomic link, so exhaustion means a pathological
/// reclaim war rather than an unbounded spin (CLAUDE.md loop-budget rule).
const RECLAIM_ATTEMPTS: u32 = 8;

/// Per-process counter that makes each acquisition's staging temp file name
/// unique, so concurrent threads acquiring different (or the same) paths
/// never collide on the sibling temp.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Acquire (or reclaim) the `lock.pid` at `path`.
///
/// Publishes the lock atomically in *content*, not just in file creation:
/// write `process::id()` to a per-call-unique sibling temp file, then
/// `hard_link` that temp onto `path`. The link is a single atomic step, so
/// the lock never exists at `path` empty and no concurrent acquirer can
/// observe a zero-length lock mid-write.
///
/// - On a successful link the lock is published carrying our pid: return
///   [`LockAcquisition::Acquired`].
/// - `AlreadyExists` means the path is taken; read + classify it. A
///   parseable, positive, live pid → [`LockAcquisition::Held`] (the caller
///   decides the live-holder policy). An unreadable / dead-pid / garbage
///   lock → emit one `tracing::warn!`, remove the stale lock, and retry the
///   link, bounded by `RECLAIM_ATTEMPTS`.
/// - Any other IO error (staging or linking) → [`LockAcquisition::WriteFailed`].
pub fn acquire_lock_pid(path: &Path) -> LockAcquisition {
    // Stage our pid into a per-call-unique sibling temp file. A sibling
    // shares the filesystem with `path`, which `hard_link` requires.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map_or_else(
        || "lock.pid".to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    let pid = process::id();
    let temp = loop {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!("{name}.tmp-{pid}-{seq}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(pid.to_string().as_bytes()) {
                    let _ = fs::remove_file(&candidate);
                    return LockAcquisition::WriteFailed(e);
                }
                break candidate;
            }
            // A leftover temp from a prior crashed acquisition reused our
            // (pid, seq) name: fall through to bump seq and retry.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return LockAcquisition::WriteFailed(e),
        }
    };

    let mut last_exists = None;
    for _ in 0..RECLAIM_ATTEMPTS {
        match fs::hard_link(&temp, path) {
            Ok(()) => {
                let _ = fs::remove_file(&temp);
                return LockAcquisition::Acquired(LockGuard {
                    path: path.to_path_buf(),
                });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if let Ok(raw) = fs::read_to_string(path)
                    && let Ok(held) = raw.trim().parse::<i32>()
                    && held > 0
                    && is_pid_alive(held)
                {
                    let _ = fs::remove_file(&temp);
                    return LockAcquisition::Held(held);
                }
                tracing::warn!(
                    path = %path.display(),
                    "reclaiming stale or garbage lock.pid",
                );
                // A NotFound here means another process already reclaimed;
                // ignore and retry the link.
                let _ = fs::remove_file(path);
                last_exists = Some(e);
            }
            Err(e) => {
                let _ = fs::remove_file(&temp);
                return LockAcquisition::WriteFailed(e);
            }
        }
    }

    let _ = fs::remove_file(&temp);
    LockAcquisition::WriteFailed(last_exists.unwrap_or_else(|| {
        IoError::new(
            ErrorKind::AlreadyExists,
            "lock.pid reclaim attempts exhausted",
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, process};

    fn temp_dir(tag: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = env::temp_dir().join(format!("aether-pid-lock-{tag}-{}-{nonce}", process::id()));
        fs::create_dir_all(&dir).expect("temp dir creates");
        dir
    }

    #[test]
    fn absent_lock_is_acquired() {
        let dir = temp_dir("absent");
        let path = dir.join("lock.pid");
        let guard = match acquire_lock_pid(&path) {
            LockAcquisition::Acquired(g) => g,
            other => panic!(
                "expected Acquired, got {}",
                match other {
                    LockAcquisition::Held(p) => format!("Held({p})"),
                    LockAcquisition::WriteFailed(e) => format!("WriteFailed({e})"),
                    LockAcquisition::Acquired(_) => unreachable!(),
                }
            ),
        };
        assert!(path.exists(), "lock.pid written");
        let contents = fs::read_to_string(&path).expect("lock.pid is readable");
        let written: u32 = contents.trim().parse().expect("pid is numeric");
        assert_eq!(written, process::id(), "our pid was written");
        drop(guard);
        assert!(!path.exists(), "LockGuard::drop removes lock.pid");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_lock_is_reclaimed() {
        let dir = temp_dir("garbage");
        let path = dir.join("lock.pid");
        fs::write(&path, b"not-a-pid").expect("write garbage lock");
        assert!(matches!(
            acquire_lock_pid(&path),
            LockAcquisition::Acquired(_)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dead_pid_lock_is_reclaimed() {
        let dir = temp_dir("dead");
        let path = dir.join("lock.pid");
        // i32::MAX is not a live process on any realistic system.
        fs::write(&path, i32::MAX.to_string().as_bytes()).expect("write dead-pid lock");
        assert!(matches!(
            acquire_lock_pid(&path),
            LockAcquisition::Acquired(_)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    // On non-unix, is_pid_alive always returns false, so the current process
    // would be treated as dead and the lock would be reclaimed — Held never
    // fires. Only test Held on unix.
    #[cfg(unix)]
    #[test]
    fn live_pid_yields_held() {
        let dir = temp_dir("live");
        let path = dir.join("lock.pid");
        let our_pid = i32::try_from(process::id()).expect("pid fits i32");
        fs::write(&path, our_pid.to_string().as_bytes()).expect("write live-pid lock");
        match acquire_lock_pid(&path) {
            LockAcquisition::Held(p) => assert_eq!(p, our_pid),
            LockAcquisition::Acquired(_) => panic!("expected Held, got Acquired"),
            LockAcquisition::WriteFailed(e) => panic!("expected Held, got WriteFailed({e})"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // Tripwire: mutual exclusion — of N concurrent acquirers of one absent
    // lock, exactly one wins Acquired and the rest see Held. The atomic
    // hard_link publishes the winner's live pid in one step, so no loser can
    // observe an empty lock and reclaim it into a second win. The predecessor
    // create_new + write_all form (empty file, pid a syscall later) failed
    // this 32/60.
    #[cfg(unix)]
    #[test]
    fn concurrent_acquirers_yield_exactly_one_winner() {
        use std::sync::Arc;
        use std::sync::Barrier;
        use std::thread;

        const K: usize = 16;

        let dir = temp_dir("concurrent");
        let path = dir.join("lock.pid");
        let our_pid = i32::try_from(process::id()).expect("pid fits i32");

        let barrier = Arc::new(Barrier::new(K));
        let handles: Vec<_> = (0..K)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                // A raw thread is the test's own concurrency harness, not an
                // engine actor, so it needs no settlement/trace umbrella.
                #[allow(
                    clippy::disallowed_methods,
                    reason = "test-only concurrency harness thread"
                )]
                thread::spawn(move || {
                    barrier.wait();
                    acquire_lock_pid(&path)
                })
            })
            .collect();

        let mut acquired = 0;
        let mut held = 0;
        let mut guards = Vec::new();
        for handle in handles {
            match handle.join().expect("acquirer thread joins") {
                LockAcquisition::Acquired(g) => {
                    acquired += 1;
                    guards.push(g);
                }
                LockAcquisition::Held(p) => {
                    assert_eq!(p, our_pid, "loser reads the winner's live pid");
                    held += 1;
                }
                LockAcquisition::WriteFailed(e) => panic!("unexpected WriteFailed({e})"),
            }
        }

        assert_eq!(acquired, 1, "exactly one acquirer wins the lock");
        assert_eq!(held, K - 1, "every other acquirer sees Held");
        drop(guards);
        let _ = fs::remove_dir_all(&dir);
    }
}
