//! Reads the journal actor hands to its hold-until-resolve worker (ADR-0093).

use std::path::PathBuf;
use std::sync::Arc;

use aether_bloomery_kinds::ClosureLimit;
use aether_data::{Blob, KindId};
use aether_substrate::actor::native::BlobCheckIn;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::Digest;
use crate::blobs::BlobDir;
use crate::closure::{Closure, plan_closure, read_slab};
use crate::journal::{BUSY_TIMEOUT, JournalError, RootLock};

/// Reads closures and single artifacts over one locked journal root on
/// whatever thread holds it.
///
/// Crate-private: the module is private and nothing re-exports it. Made by
/// [`crate::Journal::worker_reader`], and `Send`, so the journal actor moves
/// one into a hold-until-resolve worker (ADR-0093). Each read opens its own
/// read-only connection on the calling thread; WAL gives that connection every
/// transaction committed before it opened. The reader shares the root's lock,
/// so the root stays locked while a read runs.
pub struct WorkerReader {
    database: PathBuf,
    blobs: BlobDir,
    _lock: Arc<RootLock>,
}

impl WorkerReader {
    #[must_use]
    pub fn new(database: PathBuf, blobs: BlobDir, lock: Arc<RootLock>) -> Self {
        Self { database, blobs, _lock: lock }
    }

    /// As [`crate::Journal::read_closure`], over a read-only connection opened
    /// on the calling thread, with every member checked in through
    /// `check_in` as one slab: one allocation for the whole closure, each
    /// member file read straight into its region. The members are read for
    /// one `Invoke` and dropped together, which is what a slab asks for.
    ///
    /// # Errors
    ///
    /// As [`crate::Journal::read_closure`], plus [`JournalError::Backend`] when
    /// the connection cannot be opened.
    pub fn read_closure(
        &self,
        root: &Digest,
        limit: ClosureLimit,
        check_in: &BlobCheckIn,
    ) -> Result<Closure, JournalError> {
        plan_closure(&self.connect()?, *root, limit)?.read_with(|members| read_slab(&self.blobs, &members, check_in))
    }

    /// The kind and checked-in payload of the artifact stored under `digest`,
    /// or `None` when it has no row, over a read-only connection opened on
    /// the calling thread. The payload is read straight into one buffer of
    /// its exact length and checked in through `check_in` once. There is no
    /// slab: a single artifact lives and dies alone.
    ///
    /// # Errors
    ///
    /// [`JournalError::Backend`] when the connection or the row read fails,
    /// and as [`BlobDir::read_payload`] for the artifact's file.
    pub fn read_artifact(
        &self,
        digest: &Digest,
        check_in: &BlobCheckIn,
    ) -> Result<Option<(KindId, Blob)>, JournalError> {
        let Some(size) = self
            .connect()?
            .query_row(
                "SELECT size_bytes FROM artifacts WHERE digest = ?1",
                params![digest.as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };

        let size_bytes = u64::try_from(size).map_err(|_| JournalError::IntegerRange)?;
        let (kind, payload) = self.blobs.read_payload(digest, size_bytes)?;
        Ok(Some((kind, check_in.check_in(payload))))
    }

    /// A read-only connection to the root's database, opened on the calling
    /// thread and waiting [`BUSY_TIMEOUT`] on a locked database.
    fn connect(&self) -> Result<Connection, JournalError> {
        let conn = Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(conn)
    }
}
