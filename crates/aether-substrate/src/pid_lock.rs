//! Shared `lock.pid` acquisition protocol (ADR-0049 §7 / ADR-0115).
//!
//! The `lock.pid` file format — write the owning-process pid, reclaim a
//! stale or garbage lock, delete the file on graceful shutdown — is the
//! same between the ADR-0049 handle store and the ADR-0115 hub binary
//! store. This module is the single definition of that protocol, consumed
//! by both stores; each store maps the [`LockAcquisition`] result to its
//! own divergent live-holder policy.

use std::fs;
use std::io::Error as IoError;
use std::path::{Path, PathBuf};
use std::process;

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

/// Acquire (or reclaim) the `lock.pid` at `path`.
///
/// Performs read → trim → parse → classify → (reclaim and) write:
///
/// - If the file holds a parseable, positive, live pid: return
///   [`LockAcquisition::Held`] — the caller decides the live-holder policy.
/// - Otherwise (file absent, dead pid, garbage): emit one `tracing::warn!`
///   on the reclaim path, then write `process::id()` via atomic tmp+rename.
///   On success return [`LockAcquisition::Acquired`]; on write failure
///   return [`LockAcquisition::WriteFailed`].
pub fn acquire_lock_pid(path: &Path) -> LockAcquisition {
    if let Ok(raw) = fs::read_to_string(path) {
        match raw.trim().parse::<i32>() {
            Ok(pid) if pid > 0 && is_pid_alive(pid) => return LockAcquisition::Held(pid),
            _ => {
                tracing::warn!(
                    path = %path.display(),
                    "reclaiming stale or garbage lock.pid",
                );
            }
        }
    }
    match crate::atomic_write::atomic_write(path, process::id().to_string().as_bytes()) {
        Ok(()) => LockAcquisition::Acquired(LockGuard {
            path: path.to_path_buf(),
        }),
        Err(e) => LockAcquisition::WriteFailed(e),
    }
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
}
