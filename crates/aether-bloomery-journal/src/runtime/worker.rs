//! Reads the journal actor hands to its hold-until-resolve worker (ADR-0093).

use std::path::PathBuf;
use std::sync::Arc;

use aether_bloomery_kinds::{ClosureArtifact, ClosureLimit};
use aether_substrate::actor::native::BlobCheckIn;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::Digest;
use crate::runtime::blobs::BlobDir;
use crate::runtime::cache::ReadCache;
use crate::runtime::closure::{Closure, PlannedMember, Stored, plan_closure, read_slab};
use crate::runtime::journal::{BUSY_TIMEOUT, JournalError, RootLock};

/// The recorded stored length of one artifact row.
const SIZE_QUERY: &str = "SELECT size_bytes FROM artifacts WHERE digest = ?1";

/// The bytes a stored blob holds beyond its payload: its kind prefix.
const KIND_PREFIX_BYTES: u64 = 8;

/// Outcome of [`WorkerReader::read_artifacts`].
pub enum ReadPrefix {
    /// The answered prefix, in request order; never empty.
    Found(Vec<ClosureArtifact>),
    /// A digest before the cut has no stored artifact.
    Missing(Digest),
}

/// Reads closures and single artifacts over one locked journal root on
/// whatever thread holds it.
///
/// Crate-private: the module is private and nothing re-exports it. Made by
/// [`crate::Journal::worker_reader`], and `Send`, so the journal actor moves
/// one into a hold-until-resolve worker (ADR-0093). Each read opens its own
/// read-only connection on the calling thread; WAL gives that connection every
/// transaction committed before it opened. The reader shares the root's lock,
/// so the root stays locked while a read runs. Both reads consult the shared
/// [`ReadCache`] before touching a member file and fill it with the members
/// they read.
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
    /// on the calling thread. Members already in `cache` are reused without
    /// reading their files. No member is hashed: each claims the digest it
    /// was stored under, and the receiver verifies it. The misses are checked
    /// in through `check_in` as one slab of just their lengths, each member
    /// file read straight into its region, and the slab's members enter
    /// `cache` as one group, so they live and die together, which is what a
    /// slab asks for. The reply lists every member in walk order either way.
    ///
    /// # Errors
    ///
    /// As [`crate::Journal::read_closure`], except that a member whose stored
    /// bytes do not hash to its digest is answered rather than refused, plus
    /// [`JournalError::Backend`] when the connection cannot be opened.
    pub fn read_closure(
        &self,
        root: &Digest,
        limit: ClosureLimit,
        check_in: &BlobCheckIn,
        cache: &ReadCache,
    ) -> Result<Closure, JournalError> {
        plan_closure(&self.connect()?, *root, limit)?.read_with(|members| {
            let cached = cache.lookup(members.iter().map(PlannedMember::digest));
            let misses = members
                .iter()
                .zip(&cached)
                .filter(|(_, hit)| hit.is_none())
                .map(|(member, _)| member)
                .collect::<Vec<_>>();
            let fresh = if misses.is_empty() {
                Vec::new()
            } else {
                read_slab(&self.blobs, &misses, check_in)?
            };

            // `read_slab` returns one member per miss, in order, so each miss takes the next one.
            let mut read = fresh.iter().map(|member| member.artifact().clone()).collect::<Vec<_>>().into_iter();
            cache.insert(fresh);
            Ok(cached.into_iter().filter_map(|hit| hit.or_else(|| read.next())).collect())
        })
    }

    /// The artifacts stored under `digests`, in order, while their running
    /// stored length (payload plus the eight-byte kind prefix) stays within
    /// `limit`, and always the first. Members already in `cache` are reused
    /// without reading their files. The rest have their recorded sizes read
    /// over one read-only connection opened on the calling thread, and are
    /// read into one slab checked in through `check_in`, entering `cache` as
    /// one group, as [`Self::read_closure`] reads its misses. A digest with no row before the cut answers
    /// [`ReadPrefix::Missing`] and nothing is read.
    ///
    /// # Errors
    ///
    /// [`JournalError::Backend`] when the connection or a row read fails, and
    /// as [`BlobDir::read_payload`] for a member's file. A member's bytes are
    /// not hashed, so one that does not hash to its digest is answered and
    /// fails at its receiver's check.
    pub fn read_artifacts(
        &self,
        digests: &[Digest],
        limit: ClosureLimit,
        check_in: &BlobCheckIn,
        cache: &ReadCache,
    ) -> Result<ReadPrefix, JournalError> {
        let cached = cache.lookup(digests.iter().copied());
        let conn = self.connect()?;
        let mut length = conn.prepare(SIZE_QUERY)?;

        let mut total: u64 = 0;
        let mut answered = Vec::new();
        let mut misses = Vec::new();
        for (index, (digest, hit)) in digests.iter().zip(cached).enumerate() {
            let size_bytes = if let Some(artifact) = &hit {
                artifact.len().saturating_add(KIND_PREFIX_BYTES)
            } else {
                let Some(size) =
                    length.query_row(params![digest.as_bytes().as_slice()], |row| row.get::<_, i64>(0)).optional()?
                else {
                    return Ok(ReadPrefix::Missing(*digest));
                };
                u64::try_from(size).map_err(|_| JournalError::IntegerRange)?
            };
            total = total.saturating_add(size_bytes);
            if index > 0 && total > limit.get() {
                break;
            }
            if hit.is_none() {
                misses.push(PlannedMember::new(*digest, size_bytes));
            }
            answered.push(hit);
        }

        let fresh = if misses.is_empty() {
            Vec::new()
        } else {
            read_slab(&self.blobs, &misses.iter().collect::<Vec<_>>(), check_in)?
        };

        // `read_slab` returns one member per miss, in order, so each miss takes the next one.
        let mut read = fresh.iter().map(|member| member.artifact().clone()).collect::<Vec<_>>().into_iter();
        cache.insert(fresh);
        Ok(ReadPrefix::Found(answered.into_iter().filter_map(|hit| hit.or_else(|| read.next())).collect()))
    }

    /// The artifact stored under `digest`, claiming `digest` unhashed for its
    /// receiver to verify, or `None` when it has no row. A member already in
    /// `cache` is returned without opening a connection. Otherwise, over a
    /// read-only connection opened on the
    /// calling thread, the payload is read straight into one buffer of its
    /// exact length, checked in through `check_in` once, and cached as a
    /// group of its own. There is no slab: a single artifact lives and dies
    /// alone.
    ///
    /// # Errors
    ///
    /// [`JournalError::Backend`] when the connection or the row read fails,
    /// and as [`BlobDir::read_payload`] for the artifact's file.
    pub fn read_artifact(
        &self,
        digest: &Digest,
        check_in: &BlobCheckIn,
        cache: &ReadCache,
    ) -> Result<Option<ClosureArtifact>, JournalError> {
        if let Some(artifact) = cache.get(*digest) {
            return Ok(Some(artifact));
        }

        let Some(size) = self
            .connect()?
            .query_row(SIZE_QUERY, params![digest.as_bytes().as_slice()], |row| row.get::<_, i64>(0))
            .optional()?
        else {
            return Ok(None);
        };

        let size_bytes = u64::try_from(size).map_err(|_| JournalError::IntegerRange)?;
        let (kind, payload) = self.blobs.read_payload(digest, size_bytes)?;
        let member = Stored::claimed(*digest, kind, check_in.check_in(payload));
        let artifact = member.artifact().clone();
        cache.insert(vec![member]);
        Ok(Some(artifact))
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
