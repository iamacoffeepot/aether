//! SQLite-backed journal: constructors, fence, append, read, decode.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::slice;
use std::sync::Arc;

use aether_bloomery_kinds::RecordedHeadMove;
use aether_data::wire::WireDecode;
use aether_data::{Kind, KindId, Storage, StorageError};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params, params_from_iter};

use crate::Digest;
use crate::artifact::{ARTIFACTS_DDL, split_artifact};
use crate::batch::Batch;
use crate::clock::{Clock, SystemClock};
use crate::draft::Draft;
use crate::entry::{Entry, Seq};

/// Kind prefix and payload of one stored artifact, or `None` when absent.
type LoadedArtifact = Option<(KindId, Vec<u8>)>;

const ENTRIES_DDL: &str = "
CREATE TABLE IF NOT EXISTS entries (
    seq INTEGER PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (length(kind) > 0 AND length(kind) <= 256),
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

/// Append-only log of typed events and content-addressed artifacts.
pub struct Journal {
    pub(crate) conn: Connection,
    pub(crate) clock: Box<dyn Clock>,
    identity: JournalIdentity,
}

impl Journal {
    /// Open a file-backed journal with [`SystemClock`].
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] when `SQLite` cannot open the path or apply DDL.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        Self::open_with_clock(path, Box::new(SystemClock))
    }

    /// Open a file-backed journal with an injected clock.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] when `SQLite` cannot open the path or apply DDL.
    pub fn open_with_clock(path: &Path, clock: Box<dyn Clock>) -> Result<Self, JournalError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
        prepare_schema(&conn)?;
        Ok(Self { conn, clock, identity: JournalIdentity::new() })
    }

    /// Open an in-memory journal. Tests use this with a fixed clock.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] when `SQLite` cannot create the connection or schema.
    pub fn open_in_memory_with_clock(clock: Box<dyn Clock>) -> Result<Self, JournalError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        prepare_schema(&conn)?;
        Ok(Self { conn, clock, identity: JournalIdentity::new() })
    }

    /// Process-local identity of this journal allocation.
    ///
    /// Stable across moves. Distinct from any other constructed journal,
    /// including a reopen of the same file.
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
    /// inserted, every citation is verified against the expected prefix,
    /// each draft named `bloomery.head_moved` is decoded as
    /// [`aether_bloomery_kinds::RecordedHeadMove`] and its destination is
    /// verified against the recorded head kind, then events are inserted.
    /// Any refusal rolls the whole transaction back. An empty
    /// batch is `Ok` of an empty range and writes nothing. The returned
    /// range is `head+1 .. head+n+1` (end exclusive).
    ///
    /// # Errors
    ///
    /// [`AppendError::HeadMoved`] when the fence does not match.
    /// [`AppendError::DanglingRef`] when a citation or head-move destination
    /// names a digest that is neither staged nor stored.
    /// [`AppendError::PrefixMismatch`] when the cited or destination blob's
    /// prefix is not the expected kind.
    /// [`AppendError::InvalidHeadMoved`] when a draft named
    /// `bloomery.head_moved` does not decode as the canonical event.
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

        insert_staged(&tx, batch, recorded_at_millis)?;
        verify_citations(&tx, batch)?;
        let range = insert_events(&tx, head, &batch.events, recorded_at_millis)?;
        tx.commit()?;
        Ok(range)
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
        let since_i64 = sqlite_i64(since.0)?;
        let limit_i64 = i64::try_from(limit).map_err(|_| JournalError::IntegerRange)?;
        let mut stmt = self.conn.prepare(
            "SELECT seq, kind, cause, recorded_at_millis, bytes FROM entries WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since_i64, limit_i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
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
                kind,
                cause: cause.map(from_sqlite_i64).transpose()?.map(Seq),
                recorded_at_millis: from_sqlite_i64(recorded_at_millis)?,
                bytes,
            });
        }
        Ok(entries)
    }

    /// Decode `entry` as `K`. Refuses when `entry.kind` is not `K::NAME`.
    ///
    /// # Errors
    ///
    /// [`DecodeError::KindMismatch`] when the stored name is not `K::NAME`.
    /// [`DecodeError::Storage`] when TLV decode fails.
    pub fn decode<K: Storage>(entry: &Entry) -> Result<K, DecodeError> {
        if entry.kind != K::NAME {
            return Err(DecodeError::KindMismatch { expected: K::NAME, actual: entry.kind.clone() });
        }
        K::decode_storage(&entry.bytes).map(|data| data.value).map_err(DecodeError::Storage)
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
        match self.get_bytes(digest)? {
            None => Ok(None),
            Some((kind, payload)) if kind == K::ID => K::decode_storage(&payload)
                .map(|data| Some(data.value))
                .map_err(|error| GetError::Decode(DecodeError::Storage(error))),
            Some((actual, _)) => Err(GetError::PrefixMismatch { expected: K::ID, actual }),
        }
    }

    /// Load one artifact as `(kind, payload)`. `Ok(None)` when absent.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend or corrupt-blob failure.
    pub fn get_bytes(&self, digest: &Digest) -> Result<Option<(KindId, Vec<u8>)>, JournalError> {
        self.get_bytes_many(slice::from_ref(digest))?.into_iter().next().ok_or(JournalError::IntegerRange)
    }

    /// Load many artifacts in one query. Results are in input order; absent
    /// digests are `None`.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] on a backend or corrupt-blob failure, never a short result.
    pub fn get_bytes_many(&self, digests: &[Digest]) -> Result<Vec<LoadedArtifact>, JournalError> {
        if digests.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; digests.len()].join(", ");
        let sql = format!("SELECT digest, bytes FROM artifacts WHERE digest IN ({placeholders})");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(digests.iter().map(|digest| digest.as_bytes().as_slice())))?;
        let mut found = HashMap::with_capacity(digests.len());
        while let Some(row) = rows.next()? {
            let raw: Vec<u8> = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            let key = Digest::from_bytes(raw.try_into().map_err(|_| JournalError::CorruptArtifactDigest)?);
            let (kind, payload) = split_artifact(&bytes)?;
            found.insert(key, (kind, payload.to_vec()));
        }
        Ok(digests.iter().map(|digest| found.get(digest).cloned()).collect())
    }
}

fn insert_staged(tx: &Transaction<'_>, batch: &Batch, recorded_at_millis: u64) -> Result<(), JournalError> {
    if batch.staged.is_empty() {
        return Ok(());
    }
    let recorded_at = sqlite_i64(recorded_at_millis)?;
    let mut stmt = tx.prepare(
        "INSERT OR IGNORE INTO artifacts (digest, size_bytes, recorded_at_millis, bytes) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for staged in &batch.staged {
        let size_bytes = sqlite_i64(u64::try_from(staged.bytes.len()).map_err(|_| JournalError::IntegerRange)?)?;
        stmt.execute(params![staged.digest.as_bytes().as_slice(), size_bytes, recorded_at, staged.bytes])?;
    }
    Ok(())
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
            stmt.execute(params![seq_i64, draft.kind, cause, recorded_at, draft.bytes])?;
        }
    }
    let last_exclusive = first.saturating_add(u64::try_from(events.len()).map_err(|_| JournalError::IntegerRange)?);
    Ok(Seq(first)..Seq(last_exclusive))
}

fn verify_citations(tx: &Transaction<'_>, batch: &Batch) -> Result<(), AppendError> {
    let mut seen = HashSet::new();
    let mut stmt = tx.prepare("SELECT substr(bytes, 1, 8) FROM artifacts WHERE digest = ?1")?;
    for citation in batch
        .staged
        .iter()
        .flat_map(|staged| staged.citations.iter())
        .chain(batch.events.iter().flat_map(|draft| draft.cites.iter()))
    {
        let digest_bytes: [u8; 32] = citation.bytes.as_slice().try_into().map_err(|_| JournalError::CorruptCitation)?;
        verify_prefix(&mut stmt, &mut seen, digest_bytes, citation.kind)?;
    }

    for draft in &batch.events {
        if draft.kind != RecordedHeadMove::NAME {
            continue;
        }
        let event = RecordedHeadMove::decode_storage(&draft.bytes)
            .map(|data| data.value)
            .map_err(AppendError::InvalidHeadMoved)?;
        verify_prefix(&mut stmt, &mut seen, *event.to().as_bytes(), event.head().kind())?;
    }
    Ok(())
}

fn verify_prefix(
    stmt: &mut rusqlite::Statement<'_>,
    seen: &mut HashSet<([u8; 32], KindId)>,
    digest_bytes: [u8; 32],
    expected: KindId,
) -> Result<(), AppendError> {
    if !seen.insert((digest_bytes, expected)) {
        return Ok(());
    }
    let prefix: Option<Vec<u8>> = stmt.query_row(params![digest_bytes.as_slice()], |row| row.get(0)).optional()?;
    let digest = Digest::from_bytes(digest_bytes);
    match prefix {
        None => Err(AppendError::DanglingRef { digest, expected }),
        Some(bytes) if bytes.len() != 8 => Err(JournalError::CorruptArtifact.into()),
        Some(bytes) => {
            let mut cursor = bytes.as_slice();
            let actual = KindId::decode(&mut cursor).map_err(|_| JournalError::CorruptArtifact)?;
            if actual == expected {
                Ok(())
            } else {
                Err(AppendError::PrefixMismatch { digest, expected, actual })
            }
        }
    }
}

fn prepare_schema(conn: &Connection) -> Result<(), JournalError> {
    conn.execute_batch(ENTRIES_DDL)?;
    conn.execute_batch(ARTIFACTS_DDL)?;
    Ok(())
}

fn head_of(conn: &Connection) -> Result<Seq, JournalError> {
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

/// Store or schema failure.
#[derive(Debug)]
pub enum JournalError {
    /// `rusqlite` / `SQLite` failure.
    Backend(rusqlite::Error),
    /// A `u64` value does not fit in `SQLite`'s `INTEGER` (i64).
    IntegerRange,
    /// A stored artifact digest was not 32 bytes.
    CorruptArtifactDigest,
    /// A stored blob is shorter than the eight-byte kind prefix.
    CorruptArtifact,
    /// A citation's identity bytes are not 32 bytes. The journal does not
    /// pad or truncate.
    CorruptCitation,
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(f, "journal backend: {error}"),
            Self::IntegerRange => write!(f, "integer does not fit in sqlite INTEGER"),
            Self::CorruptArtifactDigest => write!(f, "stored artifact digest is not 32 bytes"),
            Self::CorruptArtifact => write!(f, "stored artifact is shorter than the eight-byte kind prefix"),
            Self::CorruptCitation => write!(f, "citation identity is not 32 bytes"),
        }
    }
}

impl Error for JournalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
            Self::IntegerRange | Self::CorruptArtifactDigest | Self::CorruptArtifact | Self::CorruptCitation => None,
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
            Self::Journal(error) => write!(f, "{error}"),
        }
    }
}

impl Error for AppendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::HeadMoved { .. } | Self::DanglingRef { .. } | Self::PrefixMismatch { .. } => None,
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

/// Failure to decode an entry as a requested kind.
#[derive(Debug)]
pub enum DecodeError {
    /// `entry.kind` was not `K::NAME`.
    KindMismatch {
        /// `K::NAME` the caller asked for.
        expected: &'static str,
        /// Name stored on the entry.
        actual: String,
    },
    /// TLV decode failed.
    Storage(StorageError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KindMismatch { expected, actual } => {
                write!(f, "entry kind {actual:?} is not {expected:?}")
            }
            Self::Storage(error) => write!(f, "failed to decode entry: {error}"),
        }
    }
}

impl Error for DecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::KindMismatch { .. } => None,
            Self::Storage(error) => Some(error),
        }
    }
}
