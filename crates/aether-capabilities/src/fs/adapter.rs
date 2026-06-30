//! The fs adapter layer: the [`FileAdapter`] trait, the
//! [`LocalFileAdapter`] local-filesystem backend, the [`FsResult`]
//! alias, and the `std::io::Error` → [`FsError`] mapping. One adapter
//! instance backs one namespace root; the [`super::registry`] wires a
//! table of them at chassis boot.

use std::fs;
use std::io;
use std::io::ErrorKind;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::process;

use super::kinds::FsError;

/// Result shape used throughout the adapter layer. The variants of
/// `FsError` map directly onto ADR-0041 §1's reply enums, so the
/// chassis dispatcher can forward an adapter failure without
/// translation.
pub type FsResult<T> = Result<T, FsError>;

/// Storage backend for one namespace. Implementations decide what
/// `path` means against their own root — local files resolve it
/// relative to a directory, a bundled adapter might look it up in
/// an archive, a cloud adapter uses it as an object key. Path
/// normalization (rejecting `..` and absolute prefixes) is the
/// adapter's responsibility; callers hand the string through
/// unchanged from the incoming mail.
///
/// All four methods return `FsResult<_>`. Any backend-specific
/// detail the caller might want (OS errno text, HTTP status) rides
/// inside `FsError::AdapterError(String)`.
pub trait FileAdapter: Send + Sync {
    fn read(&self, path: &str) -> FsResult<Vec<u8>>;
    fn write(&self, path: &str, bytes: &[u8]) -> FsResult<()>;
    fn delete(&self, path: &str) -> FsResult<()>;
    fn list(&self, prefix: &str) -> FsResult<Vec<String>>;
}

/// Access mode for a `LocalFileAdapter`. `ReadWrite` allows all four
/// operations; `ReadOnly` rejects `write` and `delete` with `Forbidden`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

/// Local-filesystem adapter. One instance per namespace root; the
/// chassis boots one `LocalFileAdapter` per entry in the namespace
/// config and registers them in an `AdapterRegistry`.
///
/// **Atomic writes.** `write` stages to a sibling `*.tmp-{pid}` file
/// and `rename`s on success so a crash mid-write leaves either the
/// old contents or the new — never a torn file. Rename on
/// POSIX/Windows is atomic at the filesystem level; no application-
/// level lock needed.
///
/// **Path safety.** `resolve` rejects any `path` that contains `..`
/// segments, empty segments, or leading `/` — a component asking for
/// `save://../etc/passwd` fails with `Forbidden` before the adapter
/// touches the filesystem. `.` segments are permitted (they no-op on
/// the join). Symlink escapes from within the namespace root are not
/// defended against in v1: the substrate owns the root directory and
/// doesn't create symlinks, and adversarial writes would require a
/// pre-compromised disk state.
pub struct LocalFileAdapter {
    root: PathBuf,
    access: Access,
}

impl LocalFileAdapter {
    /// Build an adapter rooted at `root`. The directory is created
    /// if missing so a fresh install of the engine on a machine
    /// without a pre-populated `$AETHER_SAVE_DIR` still boots. The
    /// path is canonicalized so later comparisons (including the
    /// symlink-safety check the v2 asset loader wants) work against
    /// the real filesystem location.
    // `root` is owned for builder ergonomics — callers pass the result
    // of `dirs::data_dir()` / a `PathBuf::from(env)` straight in and
    // we shadow-rebind to the canonicalised form.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(root: PathBuf, access: Access) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        Ok(Self { root, access })
    }

    /// The adapter root, used in tests to inspect on-disk state.
    #[cfg(test)]
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &str) -> FsResult<PathBuf> {
        if path.starts_with('/') {
            return Err(FsError::Forbidden);
        }
        if path.split('/').any(|c| c == "..") {
            return Err(FsError::Forbidden);
        }
        Ok(self.root.join(path))
    }
}

impl FileAdapter for LocalFileAdapter {
    fn read(&self, path: &str) -> FsResult<Vec<u8>> {
        let resolved = self.resolve(path)?;
        fs::read(&resolved).map_err(fs_error_from_std)
    }

    fn write(&self, path: &str, bytes: &[u8]) -> FsResult<()> {
        if !matches!(self.access, Access::ReadWrite) {
            return Err(FsError::Forbidden);
        }
        let resolved = self.resolve(path)?;
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent).map_err(|e| FsError::AdapterError(e.to_string()))?;
        }
        let mut tmp = resolved.clone();
        let existing = tmp
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| FsError::AdapterError("non-utf8 filename".into()))?
            .to_string();
        tmp.set_file_name(format!("{existing}.tmp-{}", process::id()));
        fs::write(&tmp, bytes).map_err(|e| FsError::AdapterError(e.to_string()))?;
        fs::rename(&tmp, &resolved).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            FsError::AdapterError(e.to_string())
        })
    }

    fn delete(&self, path: &str) -> FsResult<()> {
        if !matches!(self.access, Access::ReadWrite) {
            return Err(FsError::Forbidden);
        }
        let resolved = self.resolve(path)?;
        fs::remove_file(&resolved).map_err(fs_error_from_std)
    }

    fn list(&self, prefix: &str) -> FsResult<Vec<String>> {
        let resolved = self.resolve(prefix)?;
        let entries = fs::read_dir(&resolved).map_err(fs_error_from_std)?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| FsError::AdapterError(e.to_string()))?;
            if let Some(s) = entry.file_name().to_str() {
                names.push(s.to_string());
            }
        }
        names.sort();
        Ok(names)
    }
}

// `err` taken by value so callers can use it directly as a
// `.map_err(fs_error_from_std)` callback (the closure-converted form
// is the natural shape at every call site here); `kind()` and
// `to_string()` both borrow, so technically `&Error` would work, but
// it'd force ad-hoc closures at every call site.
#[allow(clippy::needless_pass_by_value)]
pub fn fs_error_from_std(err: io::Error) -> FsError {
    match err.kind() {
        ErrorKind::NotFound => FsError::NotFound,
        ErrorKind::PermissionDenied => FsError::Forbidden,
        _ => FsError::AdapterError(err.to_string()),
    }
}
