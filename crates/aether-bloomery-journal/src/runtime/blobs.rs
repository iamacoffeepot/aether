//! The blob directory: every artifact's prefixed bytes as one digest-named file (ADR-0220).
//!
//! `blobs/<first two hex>/<digest hex>` holds exactly the bytes the digest
//! hashes. Only a rename creates that name, so a digest-named file is complete
//! and is never rewritten. A write goes through `blobs/tmp/`, which the
//! journal sweeps at open while it holds the root's lock.
//!
//! A batch writes each blob to a temp file as it streams and only records
//! it. [`BlobDir::flush`] then runs ADR-0220's order over the whole batch:
//! every file durable, every file renamed to its digest name, every touched
//! directory durable, all before the caller commits the rows that name them.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::mem;
use std::panic;
use std::path::{Path, PathBuf};
use std::thread;

use aether_data::{Digest, KindId};
use tempfile::{NamedTempFile, TempPath};

use crate::runtime::artifact::split_artifact;
use crate::runtime::journal::JournalError;

/// The most threads [`BlobDir::flush`] fsyncs a batch's files on. The
/// filesystem journal group-commits concurrent fsyncs: on the Linux gate
/// host, fsyncing 4,104 freshly written 4 KiB files took 11.8 s serially,
/// 1.43 s 16 wide, and 0.81 s 32 wide, so 32 takes most of the gain while
/// bounding the threads 48 concurrent imports spawn.
const SYNC_WIDTH: usize = 32;

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

    /// Write `bytes` to a temp file and record it in `pending` under
    /// `digest`, unless its file already exists or is already pending, as
    /// [`BlobDir::place`] does. The caller flushes `pending` with
    /// [`BlobDir::flush`] before it commits the artifact row.
    pub fn store(&self, digest: &Digest, bytes: &[u8], pending: &mut PendingSyncs) -> Result<(), JournalError> {
        if pending.files.contains_key(digest) {
            return Ok(());
        }
        let (shard, path) = self.locate(digest);
        if path.try_exists().map_err(|error| JournalError::io(&path, error))? {
            pending.shards.insert(shard);
            return Ok(());
        }

        let mut staged = self.temp_file()?;
        staged.write_all(bytes).map_err(|error| JournalError::io(staged.path(), error))?;
        self.place(digest, staged, pending)
    }

    /// [`BlobDir::store`] for a blob that stands alone: its file is durable,
    /// renamed, and its directories durable before this returns, so the
    /// caller may commit its row at once.
    pub fn store_synced(&self, digest: &Digest, bytes: &[u8]) -> Result<(), JournalError> {
        let mut pending = PendingSyncs::default();
        self.store(digest, bytes, &mut pending)?;
        self.flush(&mut pending)
    }

    /// A new temp file in `blobs/tmp/`, deleted when it drops unplaced.
    pub fn temp_file(&self) -> Result<NamedTempFile, JournalError> {
        let tmp = self.tmp();
        NamedTempFile::new_in(&tmp).map_err(|error| JournalError::io(&tmp, error))
    }

    /// Record `staged`, a temp file holding exactly the bytes `digest`
    /// hashes, in `pending` as the file to store under `digest`. The file is
    /// closed, not synced or renamed; [`BlobDir::flush`] does both for the
    /// whole batch before the caller commits the artifact row. When the
    /// digest name already exists the temp file is deleted instead and only
    /// the shard is recorded, since the file may be one an interrupted
    /// earlier placement renamed but never made durable. When the digest is
    /// already pending in this batch the temp file is deleted too.
    pub fn place(
        &self,
        digest: &Digest,
        staged: NamedTempFile,
        pending: &mut PendingSyncs,
    ) -> Result<(), JournalError> {
        if pending.files.contains_key(digest) {
            return Ok(());
        }
        let (shard, path) = self.locate(digest);
        if path.try_exists().map_err(|error| JournalError::io(&path, error))? {
            pending.shards.insert(shard);
            return Ok(());
        }
        pending.files.insert(*digest, staged.into_temp_path());
        Ok(())
    }

    /// Make every placement `pending` recorded durable under its digest
    /// name, in ADR-0220's order, then clear it: fsync every pending file,
    /// at most [`SYNC_WIDTH`] at a time; rename each to its digest name,
    /// creating its shard when missing; then fsync `blobs/` once, so every
    /// shard created since is durable, and each touched shard once, so every
    /// file renamed into it is. The caller commits the rows after this
    /// returns. On an error the files not yet renamed are deleted.
    pub fn flush(&self, pending: &mut PendingSyncs) -> Result<(), JournalError> {
        let files = mem::take(&mut pending.files);
        sync_files(files.values(), SYNC_WIDTH)?;
        for (digest, staged) in files {
            let (shard, path) = self.locate(&digest);
            create_unsynced(&shard)?;
            pending.blobs_dir = true;
            staged.persist(&path).map_err(|error| JournalError::io(&path, error.error))?;
            pending.shards.insert(shard);
        }

        if pending.blobs_dir {
            sync_dir(&self.dir)?;
            pending.blobs_dir = false;
        }
        for shard in mem::take(&mut pending.shards) {
            sync_dir(&shard)?;
        }
        Ok(())
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

    /// The kind and payload of the file stored under `digest`, which must be
    /// `size_bytes` long. The payload is read into a buffer of exactly
    /// `size_bytes - 8` bytes, so it can be checked into the engine blob
    /// store without another copy.
    ///
    /// Fails as [`BlobDir::read_payload_into`] does, and a `size_bytes` under
    /// eight is [`JournalError::CorruptArtifact`].
    pub fn read_payload(&self, digest: &Digest, size_bytes: u64) -> Result<(KindId, Box<[u8]>), JournalError> {
        let mut payload = vec![0; payload_len(size_bytes)?].into_boxed_slice();
        let kind = self.read_payload_into(digest, size_bytes, &mut payload)?;
        Ok((kind, payload))
    }

    /// Read the file stored under `digest`, which must be `size_bytes` long,
    /// returning its kind and reading its payload straight into `payload`,
    /// which must be exactly `size_bytes - 8` bytes.
    ///
    /// A missing file is [`JournalError::MissingBlob`]; a file of any other
    /// length, a `payload` of any other length, or a file whose prefix is not
    /// a kind is [`JournalError::CorruptArtifact`].
    pub fn read_payload_into(
        &self,
        digest: &Digest,
        size_bytes: u64,
        payload: &mut [u8],
    ) -> Result<KindId, JournalError> {
        let (_, path) = self.locate(digest);
        let mut file = File::open(&path).map_err(|error| read_error(digest, &path, error))?;
        let stored = file.metadata().map_err(|error| JournalError::io(&path, error))?.len();
        let expected = u64::try_from(payload.len()).ok().and_then(|len| len.checked_add(8));
        if stored != size_bytes || expected != Some(size_bytes) {
            return Err(JournalError::CorruptArtifact);
        }

        let mut prefix = [0; 8];
        file.read_exact(&mut prefix).and_then(|()| file.read_exact(payload)).map_err(|error| match error.kind() {
            ErrorKind::UnexpectedEof => JournalError::CorruptArtifact,
            _ => read_error(digest, &path, error),
        })?;
        split_artifact(&prefix).map(|(kind, _)| kind)
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

/// A batch's placements not yet durable: the temp files to sync and rename,
/// and the directories to fsync after. [`BlobDir::flush`] runs them before
/// the batch's rows commit, so a batch of n new blobs costs n file fsyncs,
/// run side by side, plus one per touched shard plus one for `blobs/`
/// (ADR-0220). Dropping it unflushed deletes every pending temp file.
#[derive(Default)]
pub struct PendingSyncs {
    /// Each new blob's closed temp file, by the digest it is stored under.
    files: HashMap<Digest, TempPath>,
    /// `blobs/` gained or may have gained a shard entry.
    blobs_dir: bool,
    shards: HashSet<PathBuf>,
}

/// Fsync every file in `paths`, striding them across at most `width` scoped
/// threads. Each file is reopened by path: an fsync flushes the inode, so it
/// makes durable the bytes written through the handle already closed.
/// Returns the first failure once every thread has finished.
fn sync_files<'a>(paths: impl Iterator<Item = &'a TempPath>, width: usize) -> Result<(), JournalError> {
    let paths: Vec<&Path> = paths.map(|path| &**path).collect();
    let threads = width.min(paths.len());
    if threads <= 1 {
        return paths.iter().try_for_each(|path| sync_file(path));
    }
    thread::scope(|scope| {
        let mut workers = Vec::with_capacity(threads);
        for first in 0..threads {
            let paths = &paths;
            workers.push(
                scope.spawn(move || paths.iter().skip(first).step_by(threads).try_for_each(|path| sync_file(path))),
            );
        }
        workers.into_iter().try_for_each(|worker| worker.join().unwrap_or_else(|panic| panic::resume_unwind(panic)))
    })
}

/// Reopen the file at `path` and fsync it.
fn sync_file(path: &Path) -> Result<(), JournalError> {
    File::open(path).and_then(|file| file.sync_all()).map_err(|error| JournalError::io(path, error))
}

/// The payload length of a stored artifact `size_bytes` long: everything
/// after its eight-byte kind prefix. A `size_bytes` under eight is
/// [`JournalError::CorruptArtifact`], and a payload length that does not fit
/// a `usize` is [`JournalError::IntegerRange`].
pub fn payload_len(size_bytes: u64) -> Result<usize, JournalError> {
    let payload_len = size_bytes.checked_sub(8).ok_or(JournalError::CorruptArtifact)?;
    usize::try_from(payload_len).map_err(|_| JournalError::IntegerRange)
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

/// Create the directory `path` when it is missing, fsyncing nothing: the
/// caller records its parent to fsync before anything inside it is relied on.
fn create_unsynced(path: &Path) -> Result<(), JournalError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
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
