//! Shared atomic file-write helper used by the handle store, the hub binary
//! store, and the pid-lock module.
//!
//! Stages bytes to a sibling `.tmp-<pid>-<nonce>` temp file, fsyncs it, then
//! renames it over the target — the pattern described in ADR-0041's
//! `LocalFileAdapter`. Creates the parent directory lazily. On rename failure
//! the temp file is removed and the error is returned so the caller can log
//! and continue (persistence is best-effort per ADR-0049 §3).

use std::fs;
use std::io::{Write as _, Error as IoError};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Write `bytes` to `target` atomically via a sibling temp file.
///
/// The temp path is `<dir>/<name>.tmp-<pid>-<nonce>` where `<nonce>` is
/// nanoseconds since the Unix epoch (finest-grained; used only to keep
/// concurrent writes collision-free, not for ordering). On rename failure
/// the temp file is cleaned up and the [`IoError`] is returned.
pub fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), IoError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("tmp");
    let tmp = target.with_file_name(format!("{file_name}.tmp-{pid}-{nonce}"));
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
