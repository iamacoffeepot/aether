//! The blob directory: every artifact's prefixed bytes as one digest-named file (ADR-0220).
//!
//! `blobs/<first two hex>/<digest hex>` holds exactly the bytes the digest
//! hashes. Only a rename creates that name, so a digest-named file is complete
//! and is never rewritten. A write goes through `blobs/tmp/`, which the
//! journal sweeps at open while it holds the root's lock.

use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::Digest;
use crate::journal::JournalError;

/// The `blobs` directory of one journal root.
#[derive(Clone)]
pub struct BlobDir {
    dir: PathBuf,
}

impl BlobDir {
    /// The blob directory of the journal at `root`. Creates nothing.
    pub fn of_root(root: &Path) -> Self {
        Self { dir: root.join("blobs") }
    }

    /// Create `blobs/` and `blobs/tmp/` when missing, syncing each parent
    /// whose entries changed.
    pub fn create(&self) -> Result<(), JournalError> {
        create_synced(&self.dir)?;
        create_synced(&self.tmp())?;
        Ok(())
    }

    /// Delete everything a write left in `blobs/tmp/`. Only safe while the
    /// caller holds the root's lock, so no write is in flight.
    pub fn sweep_tmp(&self) -> Result<(), JournalError> {
        let tmp = self.tmp();
        for entry in fs::read_dir(&tmp).map_err(|error| JournalError::io(&tmp, error))? {
            let path = entry.map_err(|error| JournalError::io(&tmp, error))?.path();
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            removed.map_err(|error| JournalError::io(&path, error))?;
        }
        Ok(())
    }

    /// Store `bytes` under `digest` unless its file already exists: temp file
    /// in `blobs/tmp/`, fsync, rename to the digest name, fsync the shard
    /// directory. The caller commits the artifact row only after this returns.
    ///
    /// An existing file may be one an interrupted earlier store renamed but
    /// never made durable, so its shard directory is fsynced before reuse.
    pub fn store(&self, digest: &Digest, bytes: &[u8]) -> Result<(), JournalError> {
        let (shard, path) = self.locate(digest);
        if path.try_exists().map_err(|error| JournalError::io(&path, error))? {
            return sync_dir(&shard);
        }

        let mut staged = self.temp_file()?;
        staged.write_all(bytes).map_err(|error| JournalError::io(staged.path(), error))?;
        self.place(digest, staged)
    }

    /// A new temp file in `blobs/tmp/`, deleted when it drops unplaced.
    pub fn temp_file(&self) -> Result<NamedTempFile, JournalError> {
        let tmp = self.tmp();
        NamedTempFile::new_in(&tmp).map_err(|error| JournalError::io(&tmp, error))
    }

    /// Make `staged`, a temp file holding exactly the bytes `digest` hashes,
    /// the file stored under `digest`: fsync it, rename it to the digest name,
    /// fsync the shard directory. When the digest name already exists the temp
    /// file is deleted instead and the shard directory is fsynced, as
    /// [`BlobDir::store`] does. The caller commits the artifact row only
    /// after this returns.
    pub fn place(&self, digest: &Digest, staged: NamedTempFile) -> Result<(), JournalError> {
        let (shard, path) = self.locate(digest);
        if path.try_exists().map_err(|error| JournalError::io(&path, error))? {
            return sync_dir(&shard);
        }
        staged.as_file().sync_all().map_err(|error| JournalError::io(staged.path(), error))?;

        create_synced(&shard)?;
        staged.persist(&path).map_err(|error| JournalError::io(&path, error.error))?;
        sync_dir(&shard)
    }

    /// The whole file stored under `digest`, which must be `size_bytes` long.
    ///
    /// A missing file is [`JournalError::MissingBlob`]; a file of any other
    /// length is [`JournalError::CorruptArtifact`].
    pub fn read(&self, digest: &Digest, size_bytes: u64) -> Result<Vec<u8>, JournalError> {
        let (_, path) = self.locate(digest);
        let bytes = fs::read(&path).map_err(|error| read_error(digest, &path, error))?;
        if u64::try_from(bytes.len()).map_err(|_| JournalError::IntegerRange)? == size_bytes {
            Ok(bytes)
        } else {
            Err(JournalError::CorruptArtifact)
        }
    }

    /// The first eight bytes of the file stored under `digest`: its kind prefix.
    ///
    /// A missing file is [`JournalError::MissingBlob`]; a file shorter than
    /// eight bytes is [`JournalError::CorruptArtifact`].
    pub fn read_prefix(&self, digest: &Digest) -> Result<[u8; 8], JournalError> {
        let (_, path) = self.locate(digest);
        let mut prefix = [0; 8];
        File::open(&path).and_then(|mut file| file.read_exact(&mut prefix)).map_err(|error| match error.kind() {
            ErrorKind::UnexpectedEof => JournalError::CorruptArtifact,
            _ => read_error(digest, &path, error),
        })?;
        Ok(prefix)
    }

    /// The digest-named file stored under `digest`, whether or not it exists.
    pub fn path_of(&self, digest: &Digest) -> PathBuf {
        self.locate(digest).1
    }

    /// The shard directory `blobs/<first two hex>` and the digest-named file inside it.
    fn locate(&self, digest: &Digest) -> (PathBuf, PathBuf) {
        let hex = digest.to_string();
        let shard = self.dir.join(&hex[..2]);
        let path = shard.join(hex);
        (shard, path)
    }

    fn tmp(&self) -> PathBuf {
        self.dir.join("tmp")
    }
}

fn read_error(digest: &Digest, path: &Path, error: io::Error) -> JournalError {
    if error.kind() == ErrorKind::NotFound {
        JournalError::MissingBlob(*digest)
    } else {
        JournalError::io(path, error)
    }
}

/// Create the directory `path` when it is missing, then fsync its parent so
/// the entry survives a crash. The parent is fsynced for an existing
/// directory too, since an interrupted earlier create may not be durable yet.
pub fn create_synced(path: &Path) -> Result<(), JournalError> {
    match fs::create_dir(path) {
        Ok(()) => sync_dir(parent_of(path)),
        Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_dir() => sync_dir(parent_of(path)),
        Err(error) => Err(JournalError::io(path, error)),
    }
}

/// The directory holding `path`; `.` for a bare relative name.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Flush a directory's entries: a rename or create inside it is durable once this returns.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), JournalError> {
    File::open(dir).and_then(|file| file.sync_all()).map_err(|error| JournalError::io(dir, error))
}

/// std cannot open a directory as a file on this platform, so there is no
/// directory handle to flush. Every build and CI host that runs the journal
/// is Unix.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), JournalError> {
    Ok(())
}
