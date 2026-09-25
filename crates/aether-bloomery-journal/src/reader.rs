//! A read-only observer of a journal root that a live [`crate::Journal`] may hold.

use std::path::Path;

use aether_data::{KindId, Storage};
use rusqlite::{Connection, OpenFlags};

use crate::blobs::BlobDir;
use crate::journal::{DATABASE_FILE, GetError, JournalError, decode_artifact, head_of, load_artifact, read_entries};
use crate::{Digest, Entry, Seq};

/// Reads a journal root without taking its lock.
///
/// It opens `journal.sqlite` read-only and creates, sweeps, and migrates
/// nothing, so it can observe a root while the one [`crate::Journal`] that
/// writes it is live, in this process or another. `SQLite`'s WAL gives it
/// every committed transaction, and a blob file never changes once it has
/// its digest name.
pub struct JournalReader {
    conn: Connection,
    blobs: BlobDir,
}

impl JournalReader {
    /// Open the journal root at `root` for reading.
    ///
    /// # Errors
    ///
    /// [`JournalError::Backend`] when `root` holds no journal `SQLite` can open.
    pub fn open(root: &Path) -> Result<Self, JournalError> {
        let conn = Connection::open_with_flags(
            root.join(DATABASE_FILE),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self { conn, blobs: BlobDir::of_root(root) })
    }

    /// As [`crate::Journal::head`].
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend failure.
    pub fn head(&self) -> Result<Seq, JournalError> {
        head_of(&self.conn)
    }

    /// As [`crate::Journal::read`].
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend failure.
    pub fn read(&self, since: Seq, limit: usize) -> Result<Vec<Entry>, JournalError> {
        read_entries(&self.conn, since, limit)
    }

    /// As [`crate::Journal::get`].
    ///
    /// # Errors
    ///
    /// As [`crate::Journal::get`].
    pub fn get<K: Storage>(&self, digest: &Digest) -> Result<Option<K>, GetError> {
        decode_artifact(self.get_bytes(digest)?)
    }

    /// As [`crate::Journal::get_bytes`].
    ///
    /// # Errors
    ///
    /// As [`crate::Journal::get_bytes`].
    pub fn get_bytes(&self, digest: &Digest) -> Result<Option<(KindId, Vec<u8>)>, JournalError> {
        load_artifact(&self.conn, &self.blobs, digest)
    }
}
