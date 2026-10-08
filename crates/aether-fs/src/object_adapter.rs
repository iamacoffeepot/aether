//! The `objects` namespace backend: a read-only [`FileAdapter`] that reads
//! an object by its path (ADR-0163 §1).
//!
//! An [`ObjectSource`] says how a path finds its file. A packaged engine
//! holds the package's table of named objects and its `pack/objects`
//! directory: a path is a key of the table, and the file read is the one
//! named for that row's sha256. An engine with no package reads a plain
//! directory, where an object is the file at its path. Both answer the same
//! requests the same way, so a directory of built files stands in for a
//! package.
//!
//! A path is a [`NamespacePath`] and nothing looser, there is no write or
//! delete, and `list` is one level deep and answers bare names, as it does
//! in every other file namespace. A read is not re-hashed (integrity is the
//! platform's job); the engine's identity for the bytes is the hash its blob
//! store takes at check-in.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::adapter::{FileAdapter, FsResult, fs_error_from_std};
use super::kinds::FsError;
use super::named_object::{NamedObject, Sha256};
use super::namespace_path::NamespacePath;

/// How the `objects` namespace finds an object's file. Composer-supplied:
/// it is the `aether.fs` capability's `Params`, chosen by the boot path and
/// never typed by an operator.
#[derive(Debug, Clone)]
pub enum ObjectSource {
    /// An engine with no package table: an object is the file at its path
    /// under `NamespaceRoots.objects`.
    Directory,
    /// A packaged engine: an object is a row of the package's table, read
    /// from the file under `root` named for the row's sha256.
    Package {
        /// The package's object directory, `pack/objects`.
        root: PathBuf,
        /// The package's named objects, by the path each is read at.
        named: BTreeMap<NamespacePath, NamedObject>,
    },
}

/// Read-only adapter over the objects one engine can read by path.
///
/// Neither case creates or canonicalizes a directory. A directory that does
/// not exist holds no objects.
#[derive(Debug)]
pub enum ObjectAdapter {
    /// An object is the file at its path under `root`.
    Directory {
        /// The directory an object's path is joined to.
        root: PathBuf,
    },
    /// An object is a row of `named`; a request's path is only ever a key
    /// of the table and is never joined to a directory.
    Package {
        /// The directory holding each object under its sha256.
        root: PathBuf,
        /// The objects that can be read, by path.
        named: BTreeMap<NamespacePath, NamedObject>,
    },
}

impl ObjectAdapter {
    /// The adapter for `source`, where `directory` is the root the
    /// directory case reads.
    ///
    /// The package case checks its own table here: every named object must
    /// be a regular file of the recorded length under the package's object
    /// directory. Bytes are not read or hashed.
    ///
    /// # Errors
    ///
    /// A [`NamedObjectError`] naming the first named object that is absent,
    /// the wrong length, or unreadable.
    pub fn new(directory: PathBuf, source: ObjectSource) -> Result<Self, NamedObjectError> {
        match source {
            ObjectSource::Directory => Ok(Self::Directory { root: directory }),
            ObjectSource::Package { root, named } => {
                for (path, object) in &named {
                    check_named_object(&root, path, *object)?;
                }
                Ok(Self::Package { root, named })
            }
        }
    }
}

/// Check that `object` is a regular file of its recorded length under
/// `root`, by `stat` alone.
fn check_named_object(root: &Path, path: &NamespacePath, object: NamedObject) -> Result<(), NamedObjectError> {
    let NamedObject { sha256, size } = object;
    let metadata = match fs::metadata(root.join(sha256.to_hex())) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(NamedObjectError::Missing { path: path.clone(), sha256, size });
        }
        Err(source) => return Err(NamedObjectError::Io { path: path.clone(), sha256, source }),
    };

    if !metadata.is_file() {
        return Err(NamedObjectError::Missing { path: path.clone(), sha256, size });
    }
    if metadata.len() != size {
        return Err(NamedObjectError::WrongSize { path: path.clone(), sha256, expected: size, actual: metadata.len() });
    }
    Ok(())
}

/// A named object the package's table lists that its object directory does
/// not hold as recorded. Raised when the package case of `ObjectAdapter`
/// is built, so a truncated install fails at boot.
#[derive(Debug)]
pub enum NamedObjectError {
    /// No regular file is named for the object's sha256.
    Missing { path: NamespacePath, sha256: Sha256, size: u64 },
    /// The object's file exists and its length is not the recorded size.
    WrongSize { path: NamespacePath, sha256: Sha256, expected: u64, actual: u64 },
    /// The object's file could not be examined.
    Io { path: NamespacePath, sha256: Sha256, source: io::Error },
}

impl fmt::Display for NamedObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path, sha256, size } => {
                write!(f, "named object {path} ({sha256}, {size} bytes) is not in the package's object store")
            }
            Self::WrongSize { path, sha256, expected, actual } => {
                write!(f, "named object {path} ({sha256}) is {actual} bytes on disk and {expected} in the manifest")
            }
            Self::Io { path, sha256, source } => write!(f, "examine named object {path} ({sha256}): {source}"),
        }
    }
}

impl Error for NamedObjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Missing { .. } | Self::WrongSize { .. } => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// What an `aether.fs.list` on `objects` lists: the root, or the directory
/// a path names.
enum ListPrefix {
    /// The empty prefix: the namespace root.
    Root,
    /// A directory beneath the root.
    Under(NamespacePath),
}

impl ListPrefix {
    /// Read a request's prefix: empty is the root, and anything else must
    /// be a [`NamespacePath`].
    fn parse(prefix: &str) -> FsResult<Self> {
        if prefix.is_empty() {
            return Ok(Self::Root);
        }
        NamespacePath::new(prefix).map(Self::Under).map_err(|_| FsError::Forbidden)
    }

    /// The bare name of the entry `path` sits in or is, one level beneath
    /// this prefix, or `None` when `path` is not beneath it.
    fn entry_of<'path>(&self, path: &'path str) -> Option<&'path str> {
        let beneath = match self {
            Self::Root => path,
            Self::Under(directory) => path.strip_prefix(directory.as_str())?.strip_prefix('/')?,
        };
        let entry = beneath.split_once('/').map_or(beneath, |(first, _)| first);
        Some(entry)
    }

    /// What listing a directory that holds nothing answers: the root is an
    /// empty namespace, and anything beneath it is not there.
    fn when_absent(&self) -> FsResult<Vec<String>> {
        match self {
            Self::Root => Ok(Vec::new()),
            Self::Under(_) => Err(FsError::NotFound),
        }
    }
}

/// The names one level beneath `prefix` in a table of named objects.
fn list_named(named: &BTreeMap<NamespacePath, NamedObject>, prefix: &ListPrefix) -> FsResult<Vec<String>> {
    let entries: BTreeSet<&str> = named.keys().filter_map(|path| prefix.entry_of(path.as_str())).collect();
    if entries.is_empty() {
        return prefix.when_absent();
    }
    Ok(entries.into_iter().map(str::to_owned).collect())
}

/// The names one level beneath `prefix` in the directory `root`. An entry
/// whose name is not a path segment could not be read back, so it is not
/// listed.
fn list_directory(root: &Path, prefix: &ListPrefix) -> FsResult<Vec<String>> {
    let directory = match prefix {
        ListPrefix::Root => root.to_path_buf(),
        ListPrefix::Under(path) => root.join(path.as_str()),
    };
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if names_no_directory(&error) => return prefix.when_absent(),
        Err(error) => return Err(fs_error_from_std(error)),
    };

    let mut names = BTreeSet::new();
    for entry in entries {
        let name = entry.map_err(|error| FsError::AdapterError(error.to_string()))?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if NamespacePath::new(name).is_ok() {
            names.insert(name.to_owned());
        }
    }
    Ok(names.into_iter().collect())
}

/// Whether `error` says the path is not a directory that exists.
fn names_no_directory(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// Read the file at `path` under `root`. A path that names a directory
/// names no object, as it names none in a package's table.
fn read_in_directory(root: &Path, path: &NamespacePath) -> FsResult<Vec<u8>> {
    match fs::read(root.join(path.as_str())) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == ErrorKind::IsADirectory => Err(FsError::NotFound),
        Err(error) => Err(fs_error_from_std(error)),
    }
}

impl FileAdapter for ObjectAdapter {
    fn read(&self, path: &str) -> FsResult<Vec<u8>> {
        let path = NamespacePath::new(path).map_err(|_| FsError::Forbidden)?;
        match self {
            Self::Directory { root } => read_in_directory(root, &path),
            Self::Package { root, named } => {
                let object = named.get(&path).ok_or(FsError::NotFound)?;
                fs::read(root.join(object.sha256.to_hex())).map_err(fs_error_from_std)
            }
        }
    }

    fn write(&self, _path: &str, _bytes: &[u8]) -> FsResult<()> {
        Err(FsError::Forbidden)
    }

    fn delete(&self, _path: &str) -> FsResult<()> {
        Err(FsError::Forbidden)
    }

    fn list(&self, prefix: &str) -> FsResult<Vec<String>> {
        let prefix = ListPrefix::parse(prefix)?;
        match self {
            Self::Directory { root } => list_directory(root, &prefix),
            Self::Package { named, .. } => list_named(named, &prefix),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use aether_substrate::testing::{cleanup, scratch_dir};

    use super::{NamedObjectError, ObjectAdapter, ObjectSource};
    use crate::{FileAdapter, FsError, NamedObject, NamespacePath, Sha256};

    fn path(text: &str) -> NamespacePath {
        NamespacePath::new(text).expect("test setup: a well-formed path")
    }

    /// Write `bytes` under `root` as the object `fill` names, and answer
    /// its table row.
    fn store(root: &Path, fill: u8, bytes: &[u8]) -> NamedObject {
        let sha256 = Sha256([fill; 32]);
        fs::write(root.join(sha256.to_hex()), bytes).expect("test setup: object writes");
        NamedObject { sha256, size: bytes.len() as u64 }
    }

    fn package(root: &Path, named: BTreeMap<NamespacePath, NamedObject>) -> ObjectAdapter {
        ObjectAdapter::new(root.join("unread"), ObjectSource::Package { root: root.to_path_buf(), named })
            .expect("every named object is present at its recorded length")
    }

    /// The package case reads through its table and nowhere else: a file
    /// that exists under the object directory but is not listed, and the
    /// listed object's own hash name, are both `NotFound`. An adapter that
    /// fell through to the filesystem would answer their bytes.
    #[test]
    fn object_adapter_package_reads_a_named_path_and_nothing_the_table_does_not_name() {
        let root = scratch_dir("aether-fs-objects", "package-read");
        let object = store(&root, 0x11, b"named bytes");
        fs::write(root.join("unlisted"), b"reached disk").expect("test setup: unlisted file writes");
        let adapter = package(&root, BTreeMap::from([(path("modules/a.wasm"), object)]));

        assert_eq!(adapter.read("modules/a.wasm").expect("a named path reads"), b"named bytes");
        for unnamed in ["unlisted", object.sha256.to_hex().as_str(), "modules"] {
            let read = adapter.read(unnamed);
            assert!(matches!(read, Err(FsError::NotFound)), "{unnamed:?} names no object, got {read:?}");
        }
        assert!(matches!(adapter.write("modules/a.wasm", b"x"), Err(FsError::Forbidden)));
        assert!(matches!(adapter.delete("modules/a.wasm"), Err(FsError::Forbidden)));
        cleanup(&root);
    }

    /// The directory case holds a development directory to the path rule a
    /// package holds its table to: each malformed spelling but `..` names a
    /// file that exists, so a read that reached disk would answer its bytes.
    #[test]
    fn object_adapter_directory_reads_a_nested_path_and_refuses_a_malformed_one() {
        let root = scratch_dir("aether-fs-objects", "directory-read");
        fs::create_dir_all(root.join("modules")).expect("test setup: sub-directory creates");
        fs::write(root.join("modules").join("a.wasm"), b"file bytes").expect("test setup: object writes");
        for present in ["Upper", "a b"] {
            fs::write(root.join(present), b"reached disk").expect("test setup: malformed-name file writes");
        }
        let adapter =
            ObjectAdapter::new(root.clone(), ObjectSource::Directory).expect("the directory case checks nothing");

        assert_eq!(adapter.read("modules/a.wasm").expect("a nested path reads"), b"file bytes");
        for malformed in ["Upper", "a b", "modules/../Upper", "/modules/a.wasm", "modules//a.wasm", ""] {
            let read = adapter.read(malformed);
            assert!(matches!(read, Err(FsError::Forbidden)), "{malformed:?} must be refused, got {read:?}");
        }
        assert!(matches!(adapter.read("modules"), Err(FsError::NotFound)), "a directory names no object");
        assert!(matches!(adapter.write("modules/a.wasm", b"x"), Err(FsError::Forbidden)));
        assert!(matches!(adapter.delete("modules/a.wasm"), Err(FsError::Forbidden)));

        let absent = ObjectAdapter::new(root.join("never-created"), ObjectSource::Directory)
            .expect("the directory case checks nothing");
        assert!(matches!(absent.read("modules/a.wasm"), Err(FsError::NotFound)));
        assert_eq!(absent.list("").expect("an absent root lists as empty"), Vec::<String>::new());
        assert!(!root.join("never-created").exists(), "the adapter never creates its root");
        cleanup(&root);
    }

    /// One tree, as a directory of files and as a package's table: `list`
    /// answers the same for the root, a sub-directory, an unknown prefix
    /// and a malformed one. The bug is the two cases drifting apart, so a
    /// loader written against a development directory breaks when packaged.
    #[test]
    fn object_adapter_lists_a_directory_and_a_package_alike() {
        let root = scratch_dir("aether-fs-objects", "list");
        let files = root.join("files");
        let objects = root.join("objects");
        fs::create_dir_all(files.join("modules").join("deep")).expect("test setup: file tree creates");
        fs::create_dir_all(&objects).expect("test setup: object directory creates");
        let tree = ["modules/b.wasm", "modules/a.wasm", "modules/deep/c.wasm", "top.bin"];
        let mut named = BTreeMap::new();
        for (fill, text) in (1u8..).zip(tree) {
            fs::write(files.join(text), text).expect("test setup: tree file writes");
            named.insert(path(text), store(&objects, fill, text.as_bytes()));
        }
        let directory = ObjectAdapter::new(files, ObjectSource::Directory).expect("the directory case checks nothing");
        let package = package(&objects, named);

        for adapter in [&directory, &package] {
            assert_eq!(adapter.list("").expect("the root lists"), ["modules", "top.bin"], "{adapter:?}");
            assert_eq!(
                adapter.list("modules").expect("a sub-directory lists"),
                ["a.wasm", "b.wasm", "deep"],
                "{adapter:?}"
            );
            assert!(matches!(adapter.list("nowhere"), Err(FsError::NotFound)), "{adapter:?}");
            assert!(matches!(adapter.list("top.bin"), Err(FsError::NotFound)), "a file is no directory: {adapter:?}");
            assert!(matches!(adapter.list("modules/"), Err(FsError::Forbidden)), "{adapter:?}");
            assert!(matches!(adapter.list("../files"), Err(FsError::Forbidden)), "{adapter:?}");
        }
        cleanup(&root);
    }

    /// Building the package case is the boot check: a named object that is
    /// absent, or present at another length, refuses the adapter naming the
    /// object. An adapter that skipped the check, or compared nothing, would
    /// boot a truncated install and fail at the first read.
    #[test]
    fn object_adapter_package_refuses_an_absent_or_wrong_length_named_object() {
        let root = scratch_dir("aether-fs-objects", "package-check");
        let present = store(&root, 0x21, b"whole");
        let truncated = NamedObject { size: 9, ..store(&root, 0x22, b"short") };
        let absent = NamedObject { sha256: Sha256([0x23; 32]), size: 4 };
        let build = |object: NamedObject| {
            let named = BTreeMap::from([(path("a/present.bin"), present), (path("b/subject.bin"), object)]);
            ObjectAdapter::new(root.join("unread"), ObjectSource::Package { root: root.clone(), named })
        };

        let missing = build(absent).expect_err("an absent named object refuses the adapter");
        assert!(
            matches!(&missing, NamedObjectError::Missing { path, sha256, size: 4 }
                if path.as_str() == "b/subject.bin" && *sha256 == absent.sha256),
            "{missing}"
        );
        let wrong = build(truncated).expect_err("a wrong-length named object refuses the adapter");
        assert!(
            matches!(&wrong, NamedObjectError::WrongSize { path, expected: 9, actual: 5, .. }
                if path.as_str() == "b/subject.bin"),
            "{wrong}"
        );
        cleanup(&root);
    }
}
