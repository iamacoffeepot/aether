//! SQLite-backed journal: constructors, fence, append, read, decode.

use std::error::Error;
use std::fmt;
use std::ops::Range;
use std::path::Path;

use aether_data::{Kind, Storage, StorageError};
use rusqlite::{Connection, TransactionBehavior, params};

use crate::artifact::ARTIFACTS_DDL;
use crate::clock::{Clock, SystemClock};
use crate::draft::Draft;
use crate::entry::{Entry, Seq};

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

/// Append-only log of typed events and content-addressed artifacts.
pub struct Journal {
    pub(crate) conn: Connection,
    pub(crate) clock: Box<dyn Clock>,
}

impl Journal {
    /// Open a file-backed journal with [`SystemClock`].
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] when SQLite cannot open the path or apply DDL.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        Self::open_with_clock(path, Box::new(SystemClock))
    }

    /// Open a file-backed journal with an injected clock.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] when SQLite cannot open the path or apply DDL.
    pub fn open_with_clock(path: &Path, clock: Box<dyn Clock>) -> Result<Self, JournalError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
        prepare_schema(&conn)?;
        Ok(Self { conn, clock })
    }

    /// Open an in-memory journal. Tests use this with a fixed clock.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] when SQLite cannot create the connection or schema.
    pub fn open_in_memory_with_clock(clock: Box<dyn Clock>) -> Result<Self, JournalError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        prepare_schema(&conn)?;
        Ok(Self { conn, clock })
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
    /// [`AppendError::HeadMoved`] and writes nothing; any other failure leaves
    /// the head unchanged. An empty batch is `Ok` of an empty range and writes
    /// nothing. The returned range is `head+1 .. head+n+1` (end exclusive).
    ///
    /// # Errors
    ///
    /// [`AppendError::HeadMoved`] when the fence does not match.
    /// [`AppendError::Journal`] on a backend or constraint failure.
    pub fn append(&mut self, expect_head: Seq, batch: &[Draft]) -> Result<Range<Seq>, AppendError> {
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

        let first = head.0.saturating_add(1);
        {
            let mut stmt = tx.prepare(
                "INSERT INTO entries (seq, kind, cause, recorded_at_millis, bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (offset, draft) in batch.iter().enumerate() {
                let seq = first.saturating_add(u64::try_from(offset).map_err(|_| JournalError::IntegerRange)?);
                let seq_i64 = sqlite_i64(seq)?;
                let cause = draft.cause.map(|c| sqlite_i64(c.0)).transpose()?;
                let recorded_at_millis = sqlite_i64(self.clock.now_millis())?;
                stmt.execute(params![seq_i64, draft.kind, cause, recorded_at_millis, draft.bytes])?;
            }
        }
        tx.commit()?;
        let last_exclusive = first.saturating_add(u64::try_from(batch.len()).map_err(|_| JournalError::IntegerRange)?);
        Ok(Seq(first)..Seq(last_exclusive))
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

pub(crate) fn sqlite_i64(value: u64) -> Result<i64, JournalError> {
    i64::try_from(value).map_err(|_| JournalError::IntegerRange)
}

fn from_sqlite_i64(value: i64) -> Result<u64, JournalError> {
    u64::try_from(value).map_err(|_| JournalError::IntegerRange)
}

/// Store or schema failure.
#[derive(Debug)]
pub enum JournalError {
    /// rusqlite / SQLite failure.
    Backend(rusqlite::Error),
    /// A `u64` value does not fit in SQLite's `INTEGER` (i64).
    IntegerRange,
    /// A stored artifact digest was not 32 bytes.
    CorruptArtifactDigest,
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(f, "journal backend: {error}"),
            Self::IntegerRange => write!(f, "integer does not fit in sqlite INTEGER"),
            Self::CorruptArtifactDigest => write!(f, "stored artifact digest is not 32 bytes"),
        }
    }
}

impl Error for JournalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
            Self::IntegerRange | Self::CorruptArtifactDigest => None,
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
    /// Backend or constraint failure; the transaction did not commit.
    Journal(JournalError),
}

impl fmt::Display for AppendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeadMoved { actual } => write!(f, "journal head moved; actual head is {actual}"),
            Self::Journal(error) => write!(f, "{error}"),
        }
    }
}

impl Error for AppendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::HeadMoved { .. } => None,
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
