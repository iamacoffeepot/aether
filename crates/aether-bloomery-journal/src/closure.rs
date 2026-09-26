//! Breadth-first transitive closure over the stored citation edges, under a byte budget.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use aether_bloomery_kinds::{ClosureArtifact, ClosureLimit};
use aether_data::Blob;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::Digest;
use crate::blobs::BlobDir;
use crate::journal::{BUSY_TIMEOUT, JournalError, RootLock};

/// Outcome of [`crate::Journal::read_closure`].
#[derive(Debug, Clone)]
pub enum Closure {
    /// Every distinct reachable artifact: the root first, then breadth-first
    /// levels, each member's children in ascending digest byte order.
    Found(Vec<ClosureArtifact>),
    /// A member has no stored artifact. In a healthy journal only the root can be.
    Missing(Digest),
    /// The total stored blob length exceeds the limit. Nothing is returned.
    TooLarge,
}

/// Walks closures over one locked journal root on whatever thread holds it.
///
/// Crate-private: the module is private and nothing re-exports it. Made by
/// [`crate::Journal::closure_reader`], and `Send`, so the journal actor moves
/// one into a hold-until-resolve worker (ADR-0093). Each read opens its own
/// read-only connection on the calling thread; WAL gives that connection every
/// transaction committed before it opened. The reader shares the root's lock,
/// so the root stays locked while a walk runs.
pub struct ClosureReader {
    database: PathBuf,
    blobs: BlobDir,
    _lock: Arc<RootLock>,
}

impl ClosureReader {
    #[must_use]
    pub fn new(database: PathBuf, blobs: BlobDir, lock: Arc<RootLock>) -> Self {
        Self { database, blobs, _lock: lock }
    }

    /// As [`crate::Journal::read_closure`], over a read-only connection opened
    /// on the calling thread.
    ///
    /// # Errors
    ///
    /// As [`crate::Journal::read_closure`], plus [`JournalError::Backend`] when
    /// the connection cannot be opened.
    pub fn read(
        &self,
        root: &Digest,
        limit: ClosureLimit,
        check_in: impl FnMut(Box<[u8]>) -> Blob,
    ) -> Result<Closure, JournalError> {
        let conn = Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        walk_closure(&conn, &self.blobs, *root, limit, check_in)
    }
}

/// Walk `root`'s closure in one read snapshot. Each digest is enqueued at
/// most once; the running total, taken from each row's recorded size, is
/// checked before the member's file is read, so the budget bounds the walk.
/// Each member's payload is handed to `check_in` in the buffer it was read
/// into, and the member is built over the [`Blob`] that returns.
pub fn walk_closure(
    conn: &Connection,
    blobs: &BlobDir,
    root: Digest,
    limit: ClosureLimit,
    mut check_in: impl FnMut(Box<[u8]>) -> Blob,
) -> Result<Closure, JournalError> {
    let tx = conn.unchecked_transaction()?;
    let mut length = tx.prepare("SELECT size_bytes FROM artifacts WHERE digest = ?1")?;
    let mut children = tx.prepare("SELECT to_digest FROM citations WHERE from_digest = ?1 ORDER BY to_digest")?;

    let mut queue = VecDeque::from([root]);
    let mut visited = HashSet::from([root]);
    let mut total: u64 = 0;
    let mut artifacts = Vec::new();

    while let Some(digest) = queue.pop_front() {
        let key = digest.as_bytes().as_slice();
        let Some(size) = length.query_row(params![key], |row| row.get::<_, i64>(0)).optional()? else {
            return Ok(Closure::Missing(digest));
        };
        let size = u64::try_from(size).map_err(|_| JournalError::IntegerRange)?;
        total = total.saturating_add(size);
        if total > limit.get() {
            return Ok(Closure::TooLarge);
        }

        let (kind, payload) = blobs.read_payload(&digest, size)?;
        let artifact = ClosureArtifact::new(kind, check_in(payload));
        if artifact.claimed().unverified() != digest {
            return Err(JournalError::ArtifactDigestMismatch(digest));
        }
        artifacts.push(artifact);

        let mut rows = children.query(params![key])?;
        while let Some(row) = rows.next()? {
            let raw: Vec<u8> = row.get(0)?;
            let child = Digest::from_bytes(raw.try_into().map_err(|_| JournalError::CorruptCitation)?);
            if visited.insert(child) {
                queue.push_back(child);
            }
        }
    }
    Ok(Closure::Found(artifacts))
}
