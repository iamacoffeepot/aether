//! The `objects` namespace backend: a read-only [`FileAdapter`] over a
//! directory of content-addressed files, each named by the lowercase hex
//! sha256 of its bytes (ADR-0163 §1). A package roots it at
//! `pack/objects`; an unpackaged engine roots it at a plain directory of
//! hash-named files.
//!
//! The namespace is addressed by hash and nothing else: a path is one
//! object name, never a sub-path, and there is no write, delete, or list.
//! The name is a locator. A read is not re-hashed against it, as boot's
//! own read of the same store is not (integrity is the platform's job),
//! and the engine's identity for the bytes is the hash its blob store
//! takes at check-in.

use std::fs;
use std::path::PathBuf;

use super::adapter::{FileAdapter, FsResult, fs_error_from_std};
use super::kinds::FsError;

/// The length of an object name: a sha256 digest in hex.
const OBJECT_NAME_BYTES: usize = 64;

/// Read-only adapter over one directory of hash-named objects.
///
/// The root is never created or canonicalized, so a root that does not
/// exist holds no objects and every read of a well-formed name answers
/// `NotFound`.
pub struct ObjectAdapter {
    /// The directory a validated object name is joined to.
    root: PathBuf,
}

impl ObjectAdapter {
    /// An adapter over `root`. Touches no disk.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// Whether `path` is an object name: exactly 64 characters of `0-9a-f`.
/// Lowercase only, because a case-insensitive filesystem would otherwise
/// give one object several names.
fn is_object_name(path: &str) -> bool {
    let full_length = path.len() == OBJECT_NAME_BYTES;
    let lowercase_hex = path.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    full_length && lowercase_hex
}

impl FileAdapter for ObjectAdapter {
    fn read(&self, path: &str) -> FsResult<Vec<u8>> {
        if !is_object_name(path) {
            return Err(FsError::Forbidden);
        }
        fs::read(self.root.join(path)).map_err(fs_error_from_std)
    }

    fn write(&self, _path: &str, _bytes: &[u8]) -> FsResult<()> {
        Err(FsError::Forbidden)
    }

    fn delete(&self, _path: &str) -> FsResult<()> {
        Err(FsError::Forbidden)
    }

    fn list(&self, _prefix: &str) -> FsResult<Vec<String>> {
        Err(FsError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::ObjectAdapter;
    use crate::{FileAdapter, FsError};
    use aether_substrate::testing::{cleanup, scratch_dir};
    use std::fs;

    const NAME: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A name that is not 64 lowercase hex characters is refused before
    /// the filesystem is consulted: each malformed spelling but `..` names
    /// a file that exists under the root, so a read that reached disk
    /// would answer its bytes.
    #[test]
    fn object_adapter_refuses_a_malformed_name_without_touching_disk() {
        let root = scratch_dir("aether-fs-objects", "malformed");
        let uppercase = NAME.to_uppercase();
        let short = &NAME[..63];
        fs::create_dir_all(root.join("a")).expect("test setup: sub-directory creates");
        for present in [uppercase.as_str(), short, "a/b"] {
            fs::write(root.join(present), b"reached disk").expect("test setup: malformed-name file writes");
        }
        let adapter = ObjectAdapter::new(root.clone());

        for malformed in [uppercase.as_str(), short, "a/b", ".."] {
            let read = adapter.read(malformed);
            assert!(matches!(read, Err(FsError::Forbidden)), "{malformed:?} must be refused, got {read:?}");
        }
        cleanup(&root);
    }

    #[test]
    fn object_adapter_answers_not_found_for_an_absent_object_and_an_absent_root() {
        let root = scratch_dir("aether-fs-objects", "absent");

        assert!(matches!(ObjectAdapter::new(root.clone()).read(NAME), Err(FsError::NotFound)));
        assert!(matches!(ObjectAdapter::new(root.join("never-created")).read(NAME), Err(FsError::NotFound)));
        assert!(!root.join("never-created").exists(), "the adapter never creates its root");
        cleanup(&root);
    }

    #[test]
    fn object_adapter_reads_a_present_object_and_refuses_every_other_verb() {
        let root = scratch_dir("aether-fs-objects", "present");
        fs::write(root.join(NAME), b"object bytes").expect("test setup: object writes");
        let adapter = ObjectAdapter::new(root.clone());

        assert_eq!(adapter.read(NAME).expect("a present object reads"), b"object bytes");
        assert!(matches!(adapter.write(NAME, b"x"), Err(FsError::Forbidden)));
        assert!(matches!(adapter.delete(NAME), Err(FsError::Forbidden)));
        assert!(matches!(adapter.list(""), Err(FsError::Forbidden)));
        assert_eq!(fs::read(root.join(NAME)).expect("the object is still present"), b"object bytes");
        cleanup(&root);
    }
}
