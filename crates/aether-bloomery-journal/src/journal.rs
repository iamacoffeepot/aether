//! The journal over one root: constructors, fence, append, read, decode.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::{fmt, slice, str};

use aether_bloomery_kinds::{ClosureLimit, RecordedHeadMove};
use aether_data::wire::WireDecode;
use aether_data::{Blob, Citation, Kind, KindId, Storage, StorageError, storage_kind_id_from_name};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Statement, Transaction, TransactionBehavior, params, params_from_iter};

use crate::artifact::{ARTIFACTS_DDL, CITATIONS_DDL, split_artifact};
use crate::batch::Batch;
use crate::blobs::{BlobDir, create_synced};
use crate::clock::{Clock, SystemClock};
use crate::closure::{Closure, ClosureReader, walk_closure};
use crate::draft::Draft;
use crate::{DecodeError, Digest, Entry, Seq};

/// Kind prefix and payload of one stored artifact, or `None` when absent.
type LoadedArtifact = Option<(KindId, Vec<u8>)>;

/// The `SQLite` log inside a journal root.
pub const DATABASE_FILE: &str = "journal.sqlite";

/// The file a live journal holds an exclusive lock on.
const LOCK_FILE: &str = "lock";

/// How long a write waits for another connection's write transaction on the
/// same root, such as an [`crate::ArtifactBatch`] commit racing an `append`,
/// before it fails `SQLITE_BUSY`. Both writers insert rows only (blob bytes
/// land before their transaction begins), so a wait this long means a writer
/// is wedged. A [`ClosureReader`] connection waits the same bound.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

const ENTRIES_DDL: &str = "
CREATE TABLE IF NOT EXISTS entries (
    seq INTEGER PRIMARY KEY NOT NULL,
    kind BLOB NOT NULL CHECK (typeof(kind) = 'blob' AND length(kind) = 8),
    cause INTEGER,
    recorded_at_millis INTEGER NOT NULL,
    bytes BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS entries_kind ON entries (kind);
CREATE INDEX IF NOT EXISTS entries_cause ON entries (cause);
";

/// Process-local identity of one [`Journal`] allocation.
///
/// Equality is the backing allocation, not a unit value. Moving a journal
/// keeps the same identity; each constructor mints a new one. The token is not
/// persisted and has no public constructor.
#[derive(Clone)]
pub struct JournalIdentity {
    token: Arc<IdentityToken>,
}

struct IdentityToken;

impl JournalIdentity {
    fn new() -> Self {
        Self { token: Arc::new(IdentityToken) }
    }
}

impl PartialEq for JournalIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.token, &other.token)
    }
}

impl Eq for JournalIdentity {}

impl fmt::Debug for JournalIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JournalIdentity").finish_non_exhaustive()
    }
}

/// Append-only log of typed events and content-addressed artifacts, the
/// only writer of one journal root.
///
/// The root holds `journal.sqlite` and a `blobs` directory with one
/// digest-named file per artifact. The journal holds an exclusive lock on
/// the root, released when it and every [`crate::ArtifactStore`] derived from
/// it have dropped, or when its process dies. It writes through two doors
/// over that one lock: [`Journal::append`], and the streaming store
/// [`Journal::artifact_store`] hands out, whose batches insert rows through
/// the same row insert and citation check `append` runs.
/// The injected clock is `Send` so a journal can be owned by a native actor.
pub struct Journal {
    conn: Connection,
    clock: Box<dyn Clock + Send>,
    identity: JournalIdentity,
    pub(crate) root: PathBuf,
    pub(crate) blobs: BlobDir,
    /// Dropped last, so the lock outlives the connection.
    pub(crate) lock: Arc<RootLock>,
}

/// The exclusive lock on one journal root.
///
/// Shared by the [`Journal`] that took it and every store derived from it,
/// so the root stays locked, and `blobs/tmp/` unswept, while any of them
/// can still write.
pub struct RootLock {
    _file: File,
}

impl Journal {
    /// Open the journal root at `root` with [`SystemClock`].
    ///
    /// # Errors
    ///
    /// As [`Journal::open_with_clock`].
    pub fn open(root: &Path) -> Result<Self, JournalError> {
        Self::open_with_clock(root, Box::new(SystemClock))
    }

    /// Open the journal root at `root` with an injected clock.
    ///
    /// Creates the root when it is missing (its parent must exist), takes the
    /// exclusive lock, creates `blobs/` and `blobs/tmp/`, deletes whatever
    /// `blobs/tmp/` holds, then opens `journal.sqlite` and applies the DDL.
    ///
    /// # Errors
    ///
    /// [`JournalError::NotADirectory`] when `root` exists and is not a
    /// directory, such as a single-file journal. [`JournalError::Locked`] when
    /// another open journal, in this process or another, holds the root.
    /// [`JournalError::Io`] when the root's layout cannot be created or swept.
    /// [`JournalError::Backend`] when `SQLite` cannot open the log or apply DDL.
    pub fn open_with_clock(root: &Path, clock: Box<dyn Clock + Send>) -> Result<Self, JournalError> {
        create_root(root)?;
        let lock = lock_root(root)?;

        let blobs = BlobDir::of_root(root);
        blobs.create()?;
        blobs.sweep_tmp()?;

        let conn = Connection::open(root.join(DATABASE_FILE))?;
        configure_writer(&conn)?;
        prepare_schema(&conn)?;
        Ok(Self {
            conn,
            clock,
            identity: JournalIdentity::new(),
            root: root.to_path_buf(),
            blobs,
            lock: Arc::new(RootLock { _file: lock }),
        })
    }

    /// Process-local identity of this journal allocation.
    ///
    /// Stable across moves. Distinct from any other constructed journal,
    /// including a reopen of the same root.
    #[must_use]
    pub fn identity(&self) -> JournalIdentity {
        self.identity.clone()
    }

    /// Current head. `Seq(0)` when the log is empty.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend failure.
    pub fn head(&self) -> Result<Seq, JournalError> {
        head_of(&self.conn)
    }

    /// Append `batch` if `expect_head` is still the head.
    ///
    /// One `BEGIN IMMEDIATE` transaction: a stale fence returns
    /// [`AppendError::HeadMoved`] and writes nothing. Staged blobs are
    /// inserted, and a blob stored for the first time records its citation
    /// edges in the same transaction (a blob already stored keeps the edges
    /// it was first stored with), every citation is verified against the
    /// expected prefix,
    /// each draft with the `bloomery.head_moved` id is decoded as
    /// [`aether_bloomery_kinds::RecordedHeadMove`] and its destination is
    /// verified against the recorded head kind, every digest the batch
    /// requires (a `Transition`'s input and result) is checked for existence
    /// (no prefix), then events are inserted. Any refusal rolls the whole
    /// transaction back. An empty batch is `Ok` of an empty range and
    /// writes nothing. The returned range is `head+1 .. head+n+1` (end
    /// exclusive).
    ///
    /// # Errors
    ///
    /// [`AppendError::HeadMoved`] when the fence does not match.
    /// [`AppendError::DanglingRef`] when a citation or head-move destination
    /// names a digest that is neither staged nor stored.
    /// [`AppendError::PrefixMismatch`] when the cited or destination blob's
    /// prefix is not the expected kind.
    /// [`AppendError::InvalidHeadMoved`] when a draft identified as
    /// `bloomery.head_moved` does not decode as the canonical event.
    /// [`AppendError::MissingArtifact`] when a digest the batch requires
    /// is neither staged nor stored.
    /// [`AppendError::Journal`] wrapping [`JournalError::CorruptCitation`]
    /// when a citation's identity bytes are not 32 bytes.
    /// [`AppendError::Journal`] on a backend or constraint failure.
    pub fn append(&mut self, expect_head: Seq, batch: &Batch) -> Result<Range<Seq>, AppendError> {
        let recorded_at_millis = self.clock.now_millis();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head = head_of(&tx)?;
        if head != expect_head {
            return Err(AppendError::HeadMoved { actual: head });
        }
        if batch.is_empty() {
            tx.commit()?;
            let next = Seq(head.0.saturating_add(1));
            return Ok(next..next);
        }

        insert_staged(&tx, &self.blobs, batch, recorded_at_millis)?;
        let head_moves = batch.events.iter().filter(|draft| draft.kind == RecordedHeadMove::ID).map(head_move_target);
        verify_citations(
            &tx,
            &self.blobs,
            batch
                .staged
                .iter()
                .flat_map(|staged| staged.citations.iter())
                .chain(batch.events.iter().flat_map(|draft| draft.cites.iter()))
                .map(cited)
                .chain(head_moves),
            batch.required.iter().copied(),
        )?;
        let range = insert_events(&tx, head, &batch.events, recorded_at_millis)?;
        tx.commit()?;
        Ok(range)
    }

    /// Read `root` and every artifact it transitively cites, under `limit`.
    ///
    /// One read snapshot. The walk is breadth-first over the stored citation
    /// edges: the root first, then each member's children in ascending digest
    /// byte order, each distinct artifact once. The budget is the sum of each
    /// member's stored blob length (kind prefix plus payload), checked before
    /// the member's file is read; a total equal to `limit` fits. Over the limit
    /// is [`Closure::TooLarge`] and nothing is returned. Artifacts stored
    /// before the journal recorded citation edges have none, so their closure
    /// is the artifact alone.
    ///
    /// Each member's payload is handed to `check_in` in the buffer it was
    /// read into; the journal actor checks it into the engine blob store.
    ///
    /// # Errors
    ///
    /// [`JournalError::ArtifactDigestMismatch`] when a member's stored kind
    /// and payload do not hash to its digest. [`JournalError`] on a backend or
    /// corrupt-blob failure.
    pub fn read_closure(
        &self,
        root: &Digest,
        limit: ClosureLimit,
        check_in: impl FnMut(Box<[u8]>) -> Blob,
    ) -> Result<Closure, JournalError> {
        walk_closure(&self.conn, &self.blobs, *root, limit, check_in)
    }

    /// A [`ClosureReader`] over this root, for a worker thread to walk
    /// closures on its own read-only connection. It shares the root's lock,
    /// so the root stays locked while the reader lives, even after this
    /// journal drops.
    pub(crate) fn closure_reader(&self) -> ClosureReader {
        ClosureReader::new(self.root.join(DATABASE_FILE), self.blobs.clone(), Arc::clone(&self.lock))
    }

    /// Entries with `seq > since`, ascending, at most `limit`.
    ///
    /// A backend failure is `Err`, never a short result. `since` past the head
    /// is `Ok` of an empty vec.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend failure.
    pub fn read(&self, since: Seq, limit: usize) -> Result<Vec<Entry>, JournalError> {
        read_entries(&self.conn, since, limit)
    }

    /// Decode `entry` as `K`. Refuses when `entry.kind` is not `K::ID`.
    ///
    /// Forwards to [`Entry::decode`].
    ///
    /// # Errors
    ///
    /// [`DecodeError::KindMismatch`] when the stored id is not `K::ID`.
    /// [`DecodeError::SpecializationMismatch`] when the payload is a different
    /// typed specialization of that stored kind. [`DecodeError::Storage`] when
    /// TLV decode fails.
    pub fn decode<K: Storage>(entry: &Entry) -> Result<K, DecodeError> {
        entry.decode()
    }

    /// Load and decode a stored encoded artifact as `K`.
    ///
    /// `Ok(None)` when the digest was never stored. A stored blob whose prefix
    /// is not `K::ID` is [`GetError::PrefixMismatch`].
    ///
    /// # Errors
    ///
    /// [`GetError::Journal`] on a backend or corrupt-blob failure.
    /// [`GetError::PrefixMismatch`] when the prefix is not `K::ID`.
    /// [`GetError::Decode`] when the payload does not decode as `K`.
    pub fn get<K: Storage>(&self, digest: &Digest) -> Result<Option<K>, GetError> {
        decode_artifact(self.get_bytes(digest)?)
    }

    /// Load one artifact as `(kind, payload)`. `Ok(None)` when absent.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend or corrupt-blob failure, and
    /// [`JournalError::MissingBlob`] when the artifact's row has no file.
    pub fn get_bytes(&self, digest: &Digest) -> Result<Option<(KindId, Vec<u8>)>, JournalError> {
        load_artifact(&self.conn, &self.blobs, digest)
    }

    /// Load many artifacts: one query for the stored rows, then each row's
    /// file. Results are in input order; absent digests are `None`.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend or corrupt-blob failure, never a
    /// short result. A row whose file is missing is
    /// [`JournalError::MissingBlob`]; a file whose length is not the row's
    /// recorded size is [`JournalError::CorruptArtifact`].
    pub fn get_bytes_many(&self, digests: &[Digest]) -> Result<Vec<LoadedArtifact>, JournalError> {
        load_artifacts(&self.conn, &self.blobs, digests)
    }
}

/// Entries with `seq > since`, ascending, at most `limit`. Shared by
/// [`Journal::read`] and [`crate::JournalReader::read`].
pub fn read_entries(conn: &Connection, since: Seq, limit: usize) -> Result<Vec<Entry>, JournalError> {
    let since_i64 = sqlite_i64(since.0)?;
    let limit_i64 = i64::try_from(limit).map_err(|_| JournalError::IntegerRange)?;
    let mut stmt = conn.prepare(
        "SELECT seq, kind, cause, recorded_at_millis, bytes FROM entries WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![since_i64, limit_i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            decode_entry_kind(row.get_ref(1)?),
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (seq, kind, cause, recorded_at_millis, bytes) = row?;
        entries.push(Entry {
            seq: Seq(from_sqlite_i64(seq)?),
            kind: kind?,
            cause: cause.map(from_sqlite_i64).transpose()?.map(Seq),
            recorded_at_millis: from_sqlite_i64(recorded_at_millis)?,
            bytes,
        });
    }
    Ok(entries)
}

/// One artifact as `(kind, payload)`, or `None` when absent. Shared by
/// [`Journal::get_bytes`] and [`crate::JournalReader::get_bytes`].
pub fn load_artifact(conn: &Connection, blobs: &BlobDir, digest: &Digest) -> Result<LoadedArtifact, JournalError> {
    load_artifacts(conn, blobs, slice::from_ref(digest))?.into_iter().next().ok_or(JournalError::IntegerRange)
}

/// Many artifacts in input order: the stored rows' sizes in one query, then
/// each row's file.
fn load_artifacts(conn: &Connection, blobs: &BlobDir, digests: &[Digest]) -> Result<Vec<LoadedArtifact>, JournalError> {
    if digests.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; digests.len()].join(", ");
    let sql = format!("SELECT digest, size_bytes FROM artifacts WHERE digest IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(digests.iter().map(|digest| digest.as_bytes().as_slice())))?;
    let mut found = HashMap::with_capacity(digests.len());
    while let Some(row) = rows.next()? {
        let raw: Vec<u8> = row.get(0)?;
        let key = Digest::from_bytes(raw.try_into().map_err(|_| JournalError::CorruptArtifactDigest)?);
        let bytes = blobs.read(&key, from_sqlite_i64(row.get(1)?)?)?;
        let (kind, payload) = split_artifact(&bytes)?;
        found.insert(key, (kind, payload.to_vec()));
    }
    Ok(digests.iter().map(|digest| found.get(digest).cloned()).collect())
}

/// Decode a loaded artifact as `K`. Shared by [`Journal::get`] and
/// [`crate::JournalReader::get`].
pub fn decode_artifact<K: Storage>(loaded: LoadedArtifact) -> Result<Option<K>, GetError> {
    match loaded {
        None => Ok(None),
        Some((kind, payload)) if kind == K::ID => K::decode_storage(&payload)
            .map(|data| Some(data.value))
            .map_err(|error| GetError::Decode(DecodeError::Storage(error))),
        Some((actual, _)) => Err(GetError::PrefixMismatch { expected: K::ID, actual }),
    }
}

/// Create `root` when it is missing, or confirm that it is a directory.
fn create_root(root: &Path) -> Result<(), JournalError> {
    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(JournalError::NotADirectory { root: root.to_path_buf() }),
        Err(error) if error.kind() == ErrorKind::NotFound => create_synced(root),
        Err(error) => Err(JournalError::io(root, error)),
    }
}

/// Take the exclusive lock on `<root>/lock`. The lock belongs to the open
/// file description, so a second open fails in this process as well as in
/// another one.
fn lock_root(root: &Path) -> Result<File, JournalError> {
    let path = root.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| JournalError::io(&path, error))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(JournalError::Locked { root: root.to_path_buf() }),
        Err(TryLockError::Error(error)) => Err(JournalError::io(&path, error)),
    }
}

/// Store each staged blob whose row is absent: its file lands through
/// [`BlobDir::store`] before its row and citation edges are inserted, so a
/// committed row always names a complete file. A refusal later in the
/// append rolls the rows back and leaves any renamed file as a harmless
/// orphan, since the same content has the same name.
fn insert_staged(
    tx: &Transaction<'_>,
    blobs: &BlobDir,
    batch: &Batch,
    recorded_at_millis: u64,
) -> Result<(), JournalError> {
    if batch.staged.is_empty() {
        return Ok(());
    }
    let mut rows = ArtifactRows::prepare(tx, recorded_at_millis)?;
    for staged in &batch.staged {
        if rows.is_stored(&staged.digest)? {
            continue;
        }
        blobs.store(&staged.digest, &staged.bytes)?;
        let size_bytes = u64::try_from(staged.bytes.len()).map_err(|_| JournalError::IntegerRange)?;
        rows.insert(&staged.digest, size_bytes, &staged.citations)?;
    }
    Ok(())
}

/// The row-and-edges insert both write doors share: [`Journal::append`] and
/// [`crate::ArtifactBatch::commit`]. The caller inserts a row only after the
/// blob file it names is durable, and only when [`ArtifactRows::is_stored`]
/// says the row is absent, so a stored artifact keeps the citation edges it
/// was first stored with.
pub struct ArtifactRows<'tx> {
    stored: Statement<'tx>,
    insert: Statement<'tx>,
    edges: Statement<'tx>,
    recorded_at: i64,
}

impl<'tx> ArtifactRows<'tx> {
    /// Prepare the statements inside `tx`, stamping each row `recorded_at_millis`.
    pub fn prepare(tx: &'tx Transaction<'_>, recorded_at_millis: u64) -> Result<Self, JournalError> {
        Ok(Self {
            stored: tx.prepare("SELECT 1 FROM artifacts WHERE digest = ?1")?,
            insert: tx.prepare("INSERT INTO artifacts (digest, size_bytes, recorded_at_millis) VALUES (?1, ?2, ?3)")?,
            edges: tx.prepare("INSERT OR IGNORE INTO citations (from_digest, to_digest) VALUES (?1, ?2)")?,
            recorded_at: sqlite_i64(recorded_at_millis)?,
        })
    }

    /// True when `digest` already has a row, committed or inserted earlier in this transaction.
    pub fn is_stored(&mut self, digest: &Digest) -> Result<bool, JournalError> {
        Ok(self.stored.exists(params![digest.as_bytes().as_slice()])?)
    }

    /// Insert `digest`'s row, `size_bytes` long, and one edge per citation.
    pub fn insert(&mut self, digest: &Digest, size_bytes: u64, citations: &[Citation]) -> Result<(), JournalError> {
        let key = digest.as_bytes().as_slice();
        self.insert.execute(params![key, sqlite_i64(size_bytes)?, self.recorded_at])?;
        for citation in citations {
            let to: [u8; 32] = citation.bytes.as_slice().try_into().map_err(|_| JournalError::CorruptCitation)?;
            self.edges.execute(params![key, to.as_slice()])?;
        }
        Ok(())
    }
}

fn insert_events(
    tx: &Transaction<'_>,
    head: Seq,
    events: &[Draft],
    recorded_at_millis: u64,
) -> Result<Range<Seq>, JournalError> {
    let first = head.0.saturating_add(1);
    if events.is_empty() {
        return Ok(Seq(first)..Seq(first));
    }
    let recorded_at = sqlite_i64(recorded_at_millis)?;
    {
        let mut stmt = tx
            .prepare("INSERT INTO entries (seq, kind, cause, recorded_at_millis, bytes) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for (offset, draft) in events.iter().enumerate() {
            let seq = first.saturating_add(u64::try_from(offset).map_err(|_| JournalError::IntegerRange)?);
            let seq_i64 = sqlite_i64(seq)?;
            let cause = draft.cause.map(|c| sqlite_i64(c.0)).transpose()?;
            stmt.execute(params![seq_i64, draft.kind.0.to_le_bytes().as_slice(), cause, recorded_at, draft.bytes])?;
        }
    }
    let last_exclusive = first.saturating_add(u64::try_from(events.len()).map_err(|_| JournalError::IntegerRange)?);
    Ok(Seq(first)..Seq(last_exclusive))
}

/// A digest the check must find stored with an expected kind prefix: a
/// citation's target, or a head move's destination.
pub type CitedTarget = Result<([u8; 32], KindId), AppendError>;

/// A citation as the target the check must find. Its identity bytes must be
/// 32; the journal does not pad or truncate.
pub fn cited(citation: &Citation) -> CitedTarget {
    let digest_bytes = citation.bytes.as_slice().try_into().map_err(|_| JournalError::CorruptCitation)?;
    Ok((digest_bytes, citation.kind))
}

/// A `bloomery.head_moved` draft's destination as the target the check must
/// find, carrying the recorded head's kind.
fn head_move_target(draft: &Draft) -> CitedTarget {
    let event =
        RecordedHeadMove::decode_storage(&draft.bytes).map(|data| data.value).map_err(AppendError::InvalidHeadMoved)?;
    Ok((*event.to().as_bytes(), event.head().kind()))
}

/// The citation check both write doors share, run inside the write
/// transaction after its rows are inserted: every target in `cited` must be
/// stored with its expected prefix, in order, and every digest in `required`
/// must be stored at all.
pub fn verify_citations(
    tx: &Transaction<'_>,
    blobs: &BlobDir,
    cited: impl IntoIterator<Item = CitedTarget>,
    required: impl IntoIterator<Item = Digest>,
) -> Result<(), AppendError> {
    let mut seen = HashSet::new();
    let mut stmt = tx.prepare("SELECT 1 FROM artifacts WHERE digest = ?1")?;
    for target in cited {
        let (digest_bytes, expected) = target?;
        verify_prefix(&mut stmt, blobs, &mut seen, digest_bytes, expected)?;
    }
    for digest in required {
        verify_exists(&mut stmt, digest)?;
    }
    Ok(())
}

/// A row check: a required digest needs no prefix, only a stored row.
fn verify_exists(stored: &mut Statement<'_>, digest: Digest) -> Result<(), AppendError> {
    if stored.exists(params![digest.as_bytes().as_slice()])? {
        Ok(())
    } else {
        Err(AppendError::MissingArtifact { digest })
    }
}

/// The row must exist, then the first eight bytes of its file must be the
/// expected kind (ADR-0220 keeps no kind column).
fn verify_prefix(
    stored: &mut Statement<'_>,
    blobs: &BlobDir,
    seen: &mut HashSet<([u8; 32], KindId)>,
    digest_bytes: [u8; 32],
    expected: KindId,
) -> Result<(), AppendError> {
    if !seen.insert((digest_bytes, expected)) {
        return Ok(());
    }
    let digest = Digest::from_bytes(digest_bytes);
    if !stored.exists(params![digest_bytes.as_slice()])? {
        return Err(AppendError::DanglingRef { digest, expected });
    }
    let prefix = blobs.read_prefix(&digest)?;
    let actual = KindId::decode(&mut prefix.as_slice()).map_err(|_| JournalError::CorruptArtifact)?;
    if actual == expected {
        Ok(())
    } else {
        Err(AppendError::PrefixMismatch { digest, expected, actual })
    }
}

/// The pragmas every writing connection to a root's log runs: WAL, full
/// sync, and a busy timeout so one door's write transaction waits out the
/// other's instead of failing `SQLITE_BUSY`.
pub fn configure_writer(conn: &Connection) -> Result<(), JournalError> {
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

fn prepare_schema(conn: &Connection) -> Result<(), JournalError> {
    conn.execute_batch(ENTRIES_DDL)?;
    conn.execute_batch(ARTIFACTS_DDL)?;
    conn.execute_batch(CITATIONS_DDL)?;
    Ok(())
}

/// The last stored sequence, `Seq(0)` when empty. Shared by [`Journal::head`]
/// and [`crate::JournalReader::head`].
pub fn head_of(conn: &Connection) -> Result<Seq, JournalError> {
    let seq: Option<i64> = conn.query_row("SELECT MAX(seq) FROM entries", [], |row| row.get(0))?;
    match seq {
        None => Ok(Seq(0)),
        Some(value) => Ok(Seq(from_sqlite_i64(value)?)),
    }
}

fn sqlite_i64(value: u64) -> Result<i64, JournalError> {
    i64::try_from(value).map_err(|_| JournalError::IntegerRange)
}

fn from_sqlite_i64(value: i64) -> Result<u64, JournalError> {
    u64::try_from(value).map_err(|_| JournalError::IntegerRange)
}

fn decode_entry_kind(value: ValueRef<'_>) -> Result<KindId, JournalError> {
    match value {
        ValueRef::Blob(bytes) => Ok(KindId(u64::from_le_bytes(
            bytes.try_into().map_err(|_| JournalError::CorruptEntryKind("kind blob is not eight bytes"))?,
        ))),
        ValueRef::Text(bytes) => Ok(storage_kind_id_from_name(
            str::from_utf8(bytes).map_err(|_| JournalError::CorruptEntryKind("legacy kind name is not UTF-8"))?,
        )),
        _ => Err(JournalError::CorruptEntryKind("kind is neither a blob nor legacy text")),
    }
}

/// Store or schema failure.
#[derive(Debug)]
pub enum JournalError {
    /// `rusqlite` / `SQLite` failure.
    Backend(rusqlite::Error),
    /// A `u64` value does not fit in `SQLite`'s `INTEGER` (i64).
    IntegerRange,
    /// A stored artifact digest was not 32 bytes.
    CorruptArtifactDigest,
    /// A stored blob is shorter than the eight-byte kind prefix, or its file
    /// is not the length its row records.
    CorruptArtifact,
    /// A stored journal entry has an invalid kind value.
    CorruptEntryKind(&'static str),
    /// A citation's identity bytes are not 32 bytes. The journal does not
    /// pad or truncate.
    CorruptCitation,
    /// A stored artifact's kind and payload do not hash to its digest.
    ArtifactDigestMismatch(Digest),
    /// The journal root path exists and is not a directory, such as a
    /// single-file journal from before the root layout.
    NotADirectory {
        /// The path given as the root.
        root: PathBuf,
    },
    /// Another open journal, in this process or another, holds the root's lock.
    Locked {
        /// The root that is already held.
        root: PathBuf,
    },
    /// A stored artifact's row names a digest whose blob file is missing.
    MissingBlob(Digest),
    /// A streamed blob's payload is not the length it was opened with.
    BlobLength {
        /// The payload length the blob was opened with.
        expected_bytes: u64,
        /// The payload bytes written, or offered past the expected length.
        actual_bytes: u64,
    },
    /// [`Storage::encode_storage`] refused a value staged into an artifact batch.
    Encode(StorageError),
    /// A file-system operation on the journal root failed.
    Io {
        /// The path the operation touched.
        path: PathBuf,
        /// The underlying failure.
        error: io::Error,
    },
}

impl JournalError {
    pub(crate) fn io(path: &Path, error: io::Error) -> Self {
        Self::Io { path: path.to_path_buf(), error }
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(f, "journal backend: {error}"),
            Self::IntegerRange => write!(f, "integer does not fit in sqlite INTEGER"),
            Self::CorruptArtifactDigest => write!(f, "stored artifact digest is not 32 bytes"),
            Self::CorruptArtifact => {
                write!(f, "stored artifact is shorter than the eight-byte kind prefix or not its recorded size")
            }
            Self::CorruptEntryKind(reason) => write!(f, "stored entry kind is corrupt: {reason}"),
            Self::CorruptCitation => write!(f, "citation identity is not 32 bytes"),
            Self::ArtifactDigestMismatch(digest) => write!(f, "stored artifact bytes do not hash to {digest}"),
            Self::NotADirectory { root } => write!(f, "journal root {} is not a directory", root.display()),
            Self::Locked { root } => write!(f, "journal root {} is held by another open journal", root.display()),
            Self::MissingBlob(digest) => write!(f, "stored artifact {digest} has no blob file"),
            Self::BlobLength { expected_bytes, actual_bytes } => {
                write!(f, "streamed blob payload is {actual_bytes} bytes, opened as {expected_bytes}")
            }
            Self::Encode(error) => write!(f, "failed to encode staged artifact: {error}"),
            Self::Io { path, error } => write!(f, "journal root i/o at {}: {error}", path.display()),
        }
    }
}

impl Error for JournalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
            Self::Io { error, .. } => Some(error),
            Self::Encode(error) => Some(error),
            Self::IntegerRange
            | Self::CorruptArtifactDigest
            | Self::CorruptArtifact
            | Self::CorruptEntryKind(_)
            | Self::CorruptCitation
            | Self::ArtifactDigestMismatch(_)
            | Self::NotADirectory { .. }
            | Self::Locked { .. }
            | Self::MissingBlob(_)
            | Self::BlobLength { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for JournalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Backend(error)
    }
}

/// Failure to append a batch.
#[derive(Debug)]
pub enum AppendError {
    /// `expect_head` was stale; `actual` is the head that was read inside the transaction.
    HeadMoved {
        /// Head observed inside the append transaction.
        actual: Seq,
    },
    /// A citation or head-move destination names a digest that is neither staged nor stored.
    DanglingRef {
        /// Digest the event or artifact cited.
        digest: Digest,
        /// Kind the citation expected as the blob prefix.
        expected: KindId,
    },
    /// The cited or destination blob exists but its prefix is not the expected kind.
    PrefixMismatch {
        /// Digest that was looked up.
        digest: Digest,
        /// Kind the citation expected.
        expected: KindId,
        /// Kind actually prefixed on the stored blob.
        actual: KindId,
    },
    /// A draft named `bloomery.head_moved` did not decode as the canonical event.
    InvalidHeadMoved(StorageError),
    /// A digest the batch requires (a `Transition`'s input or result) is
    /// neither staged in this batch nor already stored.
    MissingArtifact {
        /// The required digest.
        digest: Digest,
    },
    /// Backend or constraint failure; the transaction did not commit.
    Journal(JournalError),
}

impl fmt::Display for AppendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeadMoved { actual } => write!(f, "journal head moved; actual head is {actual}"),
            Self::DanglingRef { digest, expected } => {
                write!(f, "dangling ref {digest} expected kind {expected}")
            }
            Self::PrefixMismatch { digest, expected, actual } => {
                write!(f, "prefix mismatch for {digest}: expected {expected}, actual {actual}")
            }
            Self::InvalidHeadMoved(error) => write!(f, "bloomery.head_moved did not decode: {error}"),
            Self::MissingArtifact { digest } => write!(f, "missing artifact {digest}"),
            Self::Journal(error) => write!(f, "{error}"),
        }
    }
}

impl Error for AppendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::HeadMoved { .. }
            | Self::DanglingRef { .. }
            | Self::PrefixMismatch { .. }
            | Self::MissingArtifact { .. } => None,
            Self::InvalidHeadMoved(error) => Some(error),
            Self::Journal(error) => Some(error),
        }
    }
}

impl From<JournalError> for AppendError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<rusqlite::Error> for AppendError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Journal(error.into())
    }
}

/// Failure to load a typed artifact.
#[derive(Debug)]
pub enum GetError {
    /// Backend or corrupt-blob failure.
    Journal(JournalError),
    /// Payload did not decode as the requested storage kind.
    Decode(DecodeError),
    /// Stored prefix is not the requested kind. Ids only; no kind names.
    PrefixMismatch {
        /// Kind the caller asked to decode.
        expected: KindId,
        /// Kind actually prefixed on the stored blob.
        actual: KindId,
    },
}

impl fmt::Display for GetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(error) => write!(f, "{error}"),
            Self::Decode(error) => write!(f, "{error}"),
            Self::PrefixMismatch { expected, actual } => {
                write!(f, "artifact prefix mismatch: expected {expected}, actual {actual}")
            }
        }
    }
}

impl Error for GetError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Journal(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::PrefixMismatch { .. } => None,
        }
    }
}

impl From<JournalError> for GetError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<DecodeError> for GetError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}
