//! The journal's second write door: a streaming artifact store (ADR-0237
//! open question 1, ADR-0220).
//!
//! [`Journal::append`] takes a [`crate::Batch`] whose blobs are whole
//! `Vec<u8>`s, on whatever thread owns the journal. An [`ArtifactStore`] is
//! derived from the open journal and can move to a worker thread: each of its
//! [`ArtifactBatch`]es streams blobs into digest-named files a chunk at a
//! time, stages encoded values such as trees, and commits their rows and
//! citation edges in one transaction through the same row insert and
//! citation check `append` runs. The store holds the root's lock, so the root
//! stays locked while any store or batch lives.

mod blob;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use aether_bloomery_kinds::{Digest, OpaqueBytes, Ref, artifact_blob, hash_bytes};
use aether_data::{Citation, Citations, Cites, Storage, StorageData};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use crate::blobs::BlobDir;
use crate::clock::{Clock, SystemClock};
use crate::journal::{
    AppendError, ArtifactRows, DATABASE_FILE, GetError, Journal, JournalError, RootLock, cited, configure_writer,
    decode_artifact, load_artifact, verify_citations,
};

pub use blob::{BlobFile, VerifiedBlob};

impl Journal {
    /// A streaming artifact store over this journal's root.
    ///
    /// The store shares the root's lock: the root stays locked while the
    /// store, any clone of it, or any of its batches lives, even after this
    /// journal drops. `append` keeps working while a store batch is open on
    /// another thread; the two doors' commits wait each other out.
    #[must_use]
    pub fn artifact_store(&self) -> ArtifactStore {
        ArtifactStore {
            database: self.root.join(DATABASE_FILE),
            blobs: self.blobs.clone(),
            lock: Arc::clone(&self.lock),
        }
    }
}

/// A handle that opens [`ArtifactBatch`]es over one journal root.
///
/// `Send + Sync + Clone`. Only [`Journal::artifact_store`] makes one, so a
/// store proves its root is open and locked.
#[derive(Clone)]
pub struct ArtifactStore {
    database: PathBuf,
    blobs: BlobDir,
    lock: Arc<RootLock>,
}

impl ArtifactStore {
    /// Open a batch on its own connection to the root's log.
    ///
    /// # Errors
    ///
    /// [`JournalError::Backend`] when `SQLite` cannot open or configure the
    /// connection.
    pub fn batch(&self) -> Result<ArtifactBatch, JournalError> {
        let conn = Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure_writer(&conn)?;
        Ok(ArtifactBatch {
            conn,
            blobs: self.blobs.clone(),
            pending: Vec::new(),
            seen: HashSet::new(),
            _lock: Arc::clone(&self.lock),
        })
    }
}

/// One import's or run's artifacts, committed together or not at all.
///
/// Every blob file is written, made durable, and renamed to its digest name
/// before [`ArtifactBatch::commit`] inserts the row that stores it. A batch
/// dropped without committing inserts nothing; any file it renamed is a
/// harmless orphan, since the same content has the same name (ADR-0220).
pub struct ArtifactBatch {
    conn: Connection,
    blobs: BlobDir,
    pending: Vec<PendingRow>,
    seen: HashSet<Digest>,
    /// Dropped last, so the lock outlives the connection.
    _lock: Arc<RootLock>,
}

/// A row [`ArtifactBatch::commit`] inserts when absent: its blob file is already durable.
struct PendingRow {
    digest: Digest,
    size_bytes: u64,
    citations: Vec<Citation>,
}

impl ArtifactBatch {
    /// Start streaming an [`OpaqueBytes`] blob whose payload is `len` bytes.
    ///
    /// The blob is written to a temp file a chunk at a time and hashed as it
    /// goes; `len` is checked, never used to size a buffer or the file.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] when the temp file cannot be created or written.
    pub fn blob(&mut self, len: u64) -> Result<BlobFile<'_>, JournalError> {
        BlobFile::open(self, len)
    }

    /// Encode `value`, prefix `K::ID`, write its blob file, and record its row
    /// with the citations its walk yields. The citations are checked at
    /// [`ArtifactBatch::commit`].
    ///
    /// # Errors
    ///
    /// [`JournalError::Encode`] when encoding fails. [`JournalError::Io`]
    /// when the blob file cannot be written.
    pub fn stage_encoded<K: Storage + Clone + Cites>(&mut self, value: &K) -> Result<Ref<K>, JournalError> {
        let mut sink = Citations::default();
        value.cites(&mut sink);
        let payload = K::encode_storage(&StorageData::from_value(value.clone())).map_err(JournalError::Encode)?;
        let bytes = artifact_blob(K::ID, &payload);
        let digest = hash_bytes(&bytes);
        self.blobs.store(&digest, &bytes)?;
        let size_bytes = u64::try_from(bytes.len()).map_err(|_| JournalError::IntegerRange)?;
        self.record(digest, size_bytes, sink.into_vec());
        Ok(Ref::from_digest(digest))
    }

    /// Stream a committed blob's payload, verified against its digest.
    ///
    /// `Ok(None)` when no committed row stores `blob`; a blob staged in this
    /// batch is not committed until [`ArtifactBatch::commit`].
    ///
    /// # Errors
    ///
    /// [`JournalError::MissingBlob`] when the row's file is gone.
    /// [`JournalError::CorruptArtifact`] when the row is shorter than a kind
    /// prefix. [`JournalError`] on a backend or file failure.
    pub fn blob_reader(&self, blob: &Ref<OpaqueBytes>) -> Result<Option<VerifiedBlob>, JournalError> {
        let digest = blob.digest();
        let size_bytes: Option<i64> = self
            .conn
            .query_row(
                "SELECT size_bytes FROM artifacts WHERE digest = ?1",
                params![digest.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        size_bytes
            .map(|size_bytes| {
                let size_bytes = u64::try_from(size_bytes).map_err(|_| JournalError::IntegerRange)?;
                VerifiedBlob::open(&self.blobs, digest, size_bytes)
            })
            .transpose()
    }

    /// Load and decode a committed artifact as `K`, as [`Journal::get`] does.
    ///
    /// # Errors
    ///
    /// As [`Journal::get`].
    pub fn get<K: Storage>(&self, digest: &Digest) -> Result<Option<K>, GetError> {
        decode_artifact(load_artifact(&self.conn, &self.blobs, digest)?)
    }

    /// Insert every recorded row that is absent, with its citation edges,
    /// then check every citation against its expected prefix, in one
    /// `IMMEDIATE` transaction. Any refusal inserts nothing. The rows are
    /// stamped from [`SystemClock`], which is for people only.
    ///
    /// # Errors
    ///
    /// [`AppendError::DanglingRef`] when a citation names a digest that is
    /// neither recorded here nor stored. [`AppendError::PrefixMismatch`] when
    /// the cited blob's prefix is not the expected kind.
    /// [`AppendError::Journal`] on a backend or constraint failure.
    pub fn commit(mut self) -> Result<(), AppendError> {
        let recorded_at_millis = SystemClock.now_millis();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut rows = ArtifactRows::prepare(&tx, recorded_at_millis)?;
            for pending in &self.pending {
                if !rows.is_stored(&pending.digest)? {
                    rows.insert(&pending.digest, pending.size_bytes, &pending.citations)?;
                }
            }
        }
        verify_citations(
            &tx,
            &self.blobs,
            self.pending.iter().flat_map(|pending| pending.citations.iter()).map(cited),
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record the row for a blob whose file is durable. The same digest twice
    /// in one batch is one row.
    fn record(&mut self, digest: Digest, size_bytes: u64, citations: Vec<Citation>) {
        if self.seen.insert(digest) {
            self.pending.push(PendingRow { digest, size_bytes, citations });
        }
    }
}
