//! The `SQLite`-backed git-object↔bloom-digest correspondence (ADR-0150, amended
//! 2026-07-18 for [#3590]; this slice [#3603]).
//!
//! The port-level [`Correspondence`] trait lives in `aether-bloomery-github` (the
//! one crate permitted to name a git object id); the host mounts the durable
//! implementation here, over the **same** `SQLite` file the
//! [`StoreCapability`](super::StoreCapability) owns — one persistence layer serves
//! the journal and the correspondence rather than the port inventing a second
//! store inside itself (the trait-in-port + host-impl seam, mirroring
//! [`GitDataApi`](aether_bloomery_github::GitDataApi)). It opens its own
//! [`rusqlite::Connection`] to the store path exactly as the executor dispatch
//! driver does (#3505), so the WAL journal serializes the rare concurrent write.
//!
//! The `git_correspondence` table is keyed both directions: the 32-byte bloom
//! digest is the primary key (forward), and a **unique** `(git_format, git_bytes)`
//! index is the reverse. Both keys being unique makes `record`'s `INSERT OR
//! REPLACE` last-writer-wins in *both* directions — re-recording either a digest
//! or a git object evicts the stale row on the other axis, so a reverse lookup can
//! never serve a digest an overwrite retired. The git side is **format-tagged
//! bytes** — `git_format` `1` for `sha1`/20, `2` for `sha256`/32 — so the schema
//! survives a SHA-256 object-format transition unchanged.
//!
//! [#3590]: https://github.com/iamacoffeepot/aether/issues/3590
//! [#3603]: https://github.com/iamacoffeepot/aether/issues/3603

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use aether_bloomery::Digest;
use aether_bloomery_github::{Correspondence, CorrespondenceError, GitObjectFormat, GitObjectId};
use rusqlite::Connection;

/// The `git_format` column value for a sha1 object id.
const FORMAT_SHA1: i64 = 1;
/// The `git_format` column value for a sha256 object id.
const FORMAT_SHA256: i64 = 2;

/// The correspondence table, applied idempotently on open. Coexists with the
/// [`StoreCapability`](super::StoreCapability) tables in the same file.
const MIGRATIONS: &str = "\
CREATE TABLE IF NOT EXISTS git_correspondence (
    digest     BLOB NOT NULL PRIMARY KEY,
    git_format INTEGER NOT NULL,
    git_bytes  BLOB NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS git_correspondence_by_object ON git_correspondence (git_format, git_bytes);
";

/// A `SQLite`-backed [`Correspondence`] over the store file. Holds its connection
/// behind a `Mutex` so the trait's `&self` methods (the source/executor backends
/// drive it behind a shared handle) can read and write.
pub struct SqliteCorrespondence {
    conn: Mutex<Connection>,
}

impl SqliteCorrespondence {
    /// Open (or create) the correspondence over the store at `path`. `":memory:"`
    /// opens a private in-memory database — the same code path, used by the
    /// default unconfigured chassis and tests.
    ///
    /// # Errors
    /// The connection could not be opened or the migration failed.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // Match the store's WAL + busy-timeout so a second connection to the same
        // file waits for the write lock rather than failing fast (see `SqliteStore`).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(MIGRATIONS)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// The `git_format` tag ↔ enum mapping, host-local to the SQLite column encoding.
fn format_to_tag(format: GitObjectFormat) -> i64 {
    match format {
        GitObjectFormat::Sha1 => FORMAT_SHA1,
        GitObjectFormat::Sha256 => FORMAT_SHA256,
    }
}

fn tag_to_format(tag: i64) -> Option<GitObjectFormat> {
    match tag {
        FORMAT_SHA1 => Some(GitObjectFormat::Sha1),
        FORMAT_SHA256 => Some(GitObjectFormat::Sha256),
        _ => None,
    }
}

// A stored `(git_format, git_bytes)` row back into a typed `GitObjectId`, or a
// decode fault when the tag is unknown or the bytes do not match the format's
// length (a corrupt row rather than a clean miss).
fn object_from_row(tag: i64, bytes: Vec<u8>) -> Result<GitObjectId, CorrespondenceError> {
    let format = tag_to_format(tag).ok_or_else(|| CorrespondenceError::new(format!("unknown git_format tag {tag}")))?;
    GitObjectId::new(format, bytes)
        .ok_or_else(|| CorrespondenceError::new("stored git_bytes length does not match its format tag"))
}

// Signature dictated by `Result::map_err`'s `FnOnce(E)` — the owned error is what
// the combinator hands in, and it is consumed into the message string.
#[allow(clippy::needless_pass_by_value, reason = "signature dictated by Result::map_err's FnOnce(E)")]
fn map_err(error: rusqlite::Error) -> CorrespondenceError {
    CorrespondenceError::new(error.to_string())
}

impl Correspondence for SqliteCorrespondence {
    fn record(&self, digest: &Digest, git: &GitObjectId) -> Result<(), CorrespondenceError> {
        // Last-writer-wins on BOTH keys: `digest` is the primary key and
        // `(git_format, git_bytes)` is a unique index, so INSERT OR REPLACE evicts a
        // stale row on either axis — a re-record is idempotent, and re-pointing a git
        // object at a new digest (or a digest at a new object) drops the prior row
        // rather than leaving a stale reverse mapping `resolve_digest` could serve.
        self.lock()
            .execute(
                "INSERT OR REPLACE INTO git_correspondence (digest, git_format, git_bytes) VALUES (?1, ?2, ?3)",
                rusqlite::params![digest.as_bytes().as_slice(), format_to_tag(git.format()), git.bytes()],
            )
            .map_err(map_err)?;
        Ok(())
    }

    #[allow(clippy::significant_drop_tightening, reason = "the guard must outlive the borrowed statement and rows")]
    fn resolve_git(&self, digest: &Digest) -> Result<Option<GitObjectId>, CorrespondenceError> {
        // Read the (at most one — digest is the primary key) row out of the guarded
        // region, then drop the lock before decoding the typed object.
        let row: Option<(i64, Vec<u8>)> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare("SELECT git_format, git_bytes FROM git_correspondence WHERE digest = ?1")
                .map_err(map_err)?;
            let mut rows = stmt
                .query_map(rusqlite::params![digest.as_bytes().as_slice()], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(map_err)?;
            rows.next().transpose().map_err(map_err)?
        };
        row.map(|(tag, bytes)| object_from_row(tag, bytes)).transpose()
    }

    #[allow(clippy::significant_drop_tightening, reason = "the guard must outlive the borrowed statement and rows")]
    fn resolve_digest(&self, git: &GitObjectId) -> Result<Option<Digest>, CorrespondenceError> {
        let bytes: Option<Vec<u8>> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare("SELECT digest FROM git_correspondence WHERE git_format = ?1 AND git_bytes = ?2 LIMIT 1")
                .map_err(map_err)?;
            let mut rows = stmt
                .query_map(rusqlite::params![format_to_tag(git.format()), git.bytes()], |row| row.get::<_, Vec<u8>>(0))
                .map_err(map_err)?;
            rows.next().transpose().map_err(map_err)?
        };
        bytes
            .map(|bytes| {
                let array: [u8; 32] =
                    bytes.try_into().map_err(|_| CorrespondenceError::new("stored digest is not 32 bytes"))?;
                Ok(Digest::from_bytes(array))
            })
            .transpose()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::Digest;
    use aether_bloomery_github::{Correspondence, GitObjectFormat, GitObjectId};

    use super::SqliteCorrespondence;

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn sha1(seed: u8) -> GitObjectId {
        GitObjectId::new(GitObjectFormat::Sha1, vec![seed; 20]).unwrap()
    }

    #[test]
    fn records_and_resolves_both_directions() {
        // Tripwire: a recorded correspondence resolves forward (digest → object)
        // and reverse (object → digest) — a real round-trip query against the
        // table, not a derive mirror.
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let (d, g) = (digest(1), sha1(2));
        store.record(&d, &g).unwrap();
        assert_eq!(store.resolve_git(&d).unwrap(), Some(g.clone()));
        assert_eq!(store.resolve_digest(&g).unwrap(), Some(d));
    }

    #[test]
    fn an_unrecorded_object_resolves_to_none() {
        // Tripwire: a never-recorded object is the clean `None` (the source port
        // maps it to `UnresolvedCorrespondence`), never a fabricated hit.
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        store.record(&digest(1), &sha1(2)).unwrap();
        assert_eq!(store.resolve_digest(&sha1(99)).unwrap(), None);
        assert_eq!(store.resolve_git(&digest(99)).unwrap(), None);
    }

    #[test]
    fn record_is_last_writer_wins_on_the_digest_key() {
        // Tripwire: re-recording a digest overwrites its object (idempotent
        // rebuild), never a second row that would make the forward resolve
        // ambiguous.
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let d = digest(1);
        store.record(&d, &sha1(2)).unwrap();
        store.record(&d, &sha1(3)).unwrap();
        assert_eq!(store.resolve_git(&d).unwrap(), Some(sha1(3)));
        // The superseded object no longer reverse-resolves to the digest.
        assert_eq!(store.resolve_digest(&sha1(2)).unwrap(), None);
    }

    #[test]
    fn record_is_last_writer_wins_on_the_object_key() {
        // Tripwire: re-pointing one git object at a new digest evicts the prior
        // reverse row. Before `(git_format, git_bytes)` was a UNIQUE index the stale
        // row survived — two rows shared the object and `resolve_digest`'s `LIMIT 1`
        // could serve the retired digest.
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let g = sha1(2);
        store.record(&digest(1), &g).unwrap();
        store.record(&digest(9), &g).unwrap();
        // The reverse resolves to the new digest, and the retired digest's forward
        // row was dropped on the object-key conflict rather than left dangling.
        assert_eq!(store.resolve_digest(&g).unwrap(), Some(digest(9)));
        assert_eq!(store.resolve_git(&digest(1)).unwrap(), None);
    }
}
