//! The archive tier: a directory root, one subdirectory per record class, and
//! the move that puts a record there without unlinking the source first.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// Which class of record a path on the tier holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordClass {
    /// A dispatch evidence directory, addressed by `{nonce}-evidence`.
    Evidence,
    /// A harness session tree, addressed by its slug.
    Session,
}

impl RecordClass {
    /// The subdirectory under the tier root that holds this class.
    #[must_use]
    pub const fn dir_name(self) -> &'static str {
        match self {
            Self::Evidence => "evidence",
            Self::Session => "sessions",
        }
    }

    /// The spelling `GET /archive` and `POST /archive` render.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Evidence => "evidence",
            Self::Session => "session",
        }
    }

    fn all() -> [Self; 2] {
        [Self::Evidence, Self::Session]
    }
}

/// One record that now lives on the tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedRecord {
    /// Evidence or session tree.
    pub class: RecordClass,
    /// The name the record was addressed by.
    pub name: String,
    /// Path on the tier.
    pub path: PathBuf,
    /// Total file bytes under the tree.
    pub bytes: u64,
}

/// Why a move did not complete. The source is still where it was.
#[derive(Debug)]
pub struct ArchiveError {
    message: String,
}

impl ArchiveError {
    fn io(path: &Path, error: &io::Error) -> Self {
        Self { message: format!("{}: {error}", path.display()) }
    }

    fn message(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ArchiveError {}

/// A configured archive-tier root.
#[derive(Clone, Debug)]
pub struct ArchiveTier {
    root: PathBuf,
}

impl ArchiveTier {
    /// Point the tier at `root`. Directories are created on the first move.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The configured root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `name` of `class` would live if it used its original name.
    #[must_use]
    pub fn path_for(&self, class: RecordClass, name: &str) -> PathBuf {
        self.class_dir(class).join(name)
    }

    /// Move `source` onto the tier under `class` / `name`.
    ///
    /// Tries a filesystem rename first and falls back to a recursive copy
    /// across filesystems. The source is unlinked only after the destination
    /// is confirmed present. A name already on the tier is disambiguated
    /// rather than overwritten. Every failure leaves the source in place.
    ///
    /// # Errors
    /// The source could not be moved, copied, or confirmed. The source is
    /// still at `source`.
    pub fn archive(&self, class: RecordClass, name: &str, source: &Path) -> Result<ArchivedRecord, ArchiveError> {
        if !source.is_dir() {
            return Err(ArchiveError::message(format!("{} is not a directory", source.display())));
        }
        let class_dir = self.class_dir(class);
        fs::create_dir_all(&class_dir).map_err(|error| ArchiveError::io(&class_dir, &error))?;
        let dest = unique_dest(&class_dir, name);
        match fs::rename(source, &dest) {
            Ok(()) => Self::confirm(class, name, &dest),
            Err(error) if is_cross_device(&error) => Self::copy_then_remove(class, name, source, &dest),
            Err(error) => Err(ArchiveError::io(source, &error)),
        }
    }

    /// Every record currently on the tier, evidence then session trees, each
    /// class sorted by name.
    ///
    /// # Errors
    /// A class subdirectory could not be read.
    pub fn list(&self) -> Result<Vec<ArchivedRecord>, ArchiveError> {
        let mut records = Vec::new();
        for class in RecordClass::all() {
            records.extend(self.list_class(class)?);
        }
        Ok(records)
    }

    fn list_class(&self, class: RecordClass) -> Result<Vec<ArchivedRecord>, ArchiveError> {
        let dir = self.class_dir(class);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(ArchiveError::io(&dir, &error)),
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()).map(str::to_owned) else {
                continue;
            };
            records.push(ArchivedRecord { class, name, bytes: tree_bytes(&path), path });
        }
        records.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(records)
    }

    fn class_dir(&self, class: RecordClass) -> PathBuf {
        self.root.join(class.dir_name())
    }

    fn copy_then_remove(
        class: RecordClass,
        name: &str,
        source: &Path,
        dest: &Path,
    ) -> Result<ArchivedRecord, ArchiveError> {
        if let Err(error) = copy_tree(source, dest) {
            let _ = remove_tree(dest);
            return Err(error);
        }
        if !dest.is_dir() {
            let _ = remove_tree(dest);
            return Err(ArchiveError::message(format!(
                "copied {} but {} is not a directory",
                source.display(),
                dest.display()
            )));
        }
        let source_bytes = tree_bytes(source);
        let dest_bytes = tree_bytes(dest);
        if dest_bytes != source_bytes {
            let _ = remove_tree(dest);
            return Err(ArchiveError::message(format!(
                "copied {} ({} bytes) to {} ({} bytes); source left in place",
                source.display(),
                source_bytes,
                dest.display(),
                dest_bytes
            )));
        }
        if let Err(error) = fs::remove_dir_all(source) {
            let _ = remove_tree(dest);
            return Err(ArchiveError::io(source, &error));
        }
        Ok(ArchivedRecord { class, name: name.to_owned(), path: dest.to_path_buf(), bytes: dest_bytes })
    }

    fn confirm(class: RecordClass, name: &str, dest: &Path) -> Result<ArchivedRecord, ArchiveError> {
        if !dest.is_dir() {
            return Err(ArchiveError::message(format!("{} is not a directory after the move", dest.display())));
        }
        Ok(ArchivedRecord { class, name: name.to_owned(), path: dest.to_path_buf(), bytes: tree_bytes(dest) })
    }
}

fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let mut n = 2_u32;
    loop {
        let candidate = dir.join(format!("{name}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
        n = n.saturating_add(1);
        if n == u32::MAX {
            return dir.join(format!("{name}-{n}"));
        }
    }
}

fn is_cross_device(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::CrossesDevices
}

/// Copy `source` onto `dest` iteratively. `dest` must not already exist.
fn copy_tree(source: &Path, dest: &Path) -> Result<(), ArchiveError> {
    fs::create_dir(dest).map_err(|error| ArchiveError::io(dest, &error))?;
    let mut stack = vec![(source.to_path_buf(), dest.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        let entries = fs::read_dir(&from).map_err(|error| ArchiveError::io(&from, &error))?;
        for entry in entries {
            let entry = entry.map_err(|error| ArchiveError::io(&from, &error))?;
            let from_child = entry.path();
            let name = entry.file_name();
            let to_child = to.join(&name);
            let file_type = entry.file_type().map_err(|error| ArchiveError::io(&from_child, &error))?;
            if file_type.is_dir() {
                fs::create_dir(&to_child).map_err(|error| ArchiveError::io(&to_child, &error))?;
                stack.push((from_child, to_child));
            } else if file_type.is_symlink() {
                let target = fs::read_link(&from_child).map_err(|error| ArchiveError::io(&from_child, &error))?;
                symlink(&target, &to_child).map_err(|error| ArchiveError::io(&to_child, &error))?;
            } else {
                fs::copy(&from_child, &to_child).map_err(|error| ArchiveError::io(&from_child, &error))?;
            }
        }
    }
    Ok(())
}

fn remove_tree(path: &Path) -> bool {
    match fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn tree_bytes(path: &Path) -> u64 {
    let mut bytes: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    bytes
}
