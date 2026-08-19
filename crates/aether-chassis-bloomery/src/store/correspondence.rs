//! The `SQLite`-backed backend-object↔bloom-digest correspondence.
//!
//! The domain-owned [`Correspondence`] treats backend object bytes as opaque;
//! concrete adapters validate them at their boundary. The host mounts the
//! durable implementation here, over the **same** `SQLite` file the
//! [`StoreCapability`](super::StoreCapability) owns — one persistence layer serves
//! the journal and the correspondence rather than the port inventing a second
//! store inside itself (the trait-in-port + host-impl seam, mirroring
//! the other port implementations). It opens its own [`rusqlite::Connection`]
//! to the store path exactly as the executor dispatch
//! reactor does (#3505), so the WAL journal serializes the rare concurrent write.
//!
//! The `backend_correspondence` table is keyed both directions: the 32-byte
//! bloom digest is the primary key and the opaque backend object is unique.
//! `INSERT OR REPLACE` is therefore last-writer-wins on both axes. On open, one
//! transaction creates the generic table and copies any legacy
//! `git_correspondence.git_bytes` rows without interpreting their format tag;
//! only a successful copy drops the legacy table.
//!
//! [#3590]: https://github.com/iamacoffeepot/aether/issues/3590
//! [#3603]: https://github.com/iamacoffeepot/aether/issues/3603

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use aether_bloomery::{BackendObjectId, Correspondence, CorrespondenceError, Digest};
use rusqlite::Connection;

/// The correspondence table, applied idempotently on open. Coexists with the
/// [`StoreCapability`](super::StoreCapability) tables in the same file.
const MIGRATIONS: &str = "\
CREATE TABLE IF NOT EXISTS backend_correspondence (
    digest         BLOB NOT NULL PRIMARY KEY,
    backend_object BLOB NOT NULL UNIQUE
);
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
        let mut conn = Connection::open(path)?;
        // Match the store's WAL + busy-timeout so a second connection to the same
        // file waits for the write lock rather than failing fast (see `SqliteStore`).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let transaction = conn.transaction()?;
        transaction.execute_batch(MIGRATIONS)?;
        let legacy_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'git_correspondence')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if legacy_exists {
            transaction.execute(
                "INSERT OR REPLACE INTO backend_correspondence (digest, backend_object) \
                 SELECT digest, git_bytes FROM git_correspondence",
                [],
            )?;
            transaction.execute("DROP TABLE git_correspondence", [])?;
        }
        transaction.commit()?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// Signature dictated by `Result::map_err`'s `FnOnce(E)` — the owned error is what
// the combinator hands in, and it is consumed into the message string.
#[allow(clippy::needless_pass_by_value, reason = "signature dictated by Result::map_err's FnOnce(E)")]
fn map_err(error: rusqlite::Error) -> CorrespondenceError {
    CorrespondenceError::new(error.to_string())
}

fn load_pairs(conn: &Connection) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CorrespondenceError> {
    let mut stmt = conn.prepare("SELECT digest, backend_object FROM backend_correspondence").map_err(map_err)?;
    stmt.query_map([], |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)))
        .map_err(map_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_err)
}

impl Correspondence for SqliteCorrespondence {
    fn record(&self, digest: &Digest, object: &BackendObjectId) -> Result<(), CorrespondenceError> {
        // Last-writer-wins on BOTH keys: `digest` is the primary key and
        // `backend_object` is unique, so INSERT OR REPLACE evicts a
        // stale row on either axis — a re-record is idempotent, and re-pointing a git
        // object at a new digest (or a digest at a new object) drops the prior row
        // rather than leaving a stale reverse mapping `resolve_digest` could serve.
        self.lock()
            .execute(
                "INSERT OR REPLACE INTO backend_correspondence (digest, backend_object) VALUES (?1, ?2)",
                rusqlite::params![digest.as_bytes().as_slice(), object.as_bytes()],
            )
            .map_err(map_err)?;
        Ok(())
    }

    #[allow(clippy::significant_drop_tightening, reason = "the guard must outlive the borrowed statement and rows")]
    fn resolve_backend_object(&self, digest: &Digest) -> Result<Option<BackendObjectId>, CorrespondenceError> {
        // Read the (at most one — digest is the primary key) row out of the guarded
        // region, then drop the lock before decoding the typed object.
        let bytes: Option<Vec<u8>> = {
            let conn = self.lock();
            let mut stmt =
                conn.prepare("SELECT backend_object FROM backend_correspondence WHERE digest = ?1").map_err(map_err)?;
            let mut rows = stmt
                .query_map(rusqlite::params![digest.as_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))
                .map_err(map_err)?;
            rows.next().transpose().map_err(map_err)?
        };
        Ok(bytes.map(BackendObjectId::new))
    }

    #[allow(clippy::significant_drop_tightening, reason = "the guard must outlive the borrowed statement and rows")]
    fn resolve_digest(&self, object: &BackendObjectId) -> Result<Option<Digest>, CorrespondenceError> {
        let bytes: Option<Vec<u8>> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare("SELECT digest FROM backend_correspondence WHERE backend_object = ?1 LIMIT 1")
                .map_err(map_err)?;
            let mut rows = stmt
                .query_map(rusqlite::params![object.as_bytes()], |row| row.get::<_, Vec<u8>>(0))
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

    fn pairs(&self) -> Result<Vec<(Digest, BackendObjectId)>, CorrespondenceError> {
        load_pairs(&self.lock())?
            .into_iter()
            .map(|(digest, object)| {
                let array: [u8; 32] =
                    digest.try_into().map_err(|_| CorrespondenceError::new("stored digest is not 32 bytes"))?;
                Ok((Digest::from_bytes(array), BackendObjectId::new(object)))
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::testing::digest;
    use aether_bloomery::{BackendObjectId, Correspondence};
    use rusqlite::Connection;

    use super::SqliteCorrespondence;

    fn object(seed: u8, bytes: usize) -> BackendObjectId {
        BackendObjectId::new(vec![seed; bytes])
    }

    #[test]
    fn records_and_resolves_both_directions() {
        // Tripwire: a recorded correspondence resolves forward (digest → object)
        // and reverse (object → digest) — a real round-trip query against the
        // table, not a derive mirror.
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let (digest, object) = (digest(1), object(2, 17));
        store.record(&digest, &object).unwrap();
        assert_eq!(store.resolve_backend_object(&digest).unwrap(), Some(object.clone()));
        assert_eq!(store.resolve_digest(&object).unwrap(), Some(digest));
    }

    #[test]
    fn an_unrecorded_object_resolves_to_none() {
        // Tripwire: a never-recorded object is the clean `None` (the source port
        // maps it to `UnresolvedCorrespondence`), never a fabricated hit.
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        store.record(&digest(1), &object(2, 20)).unwrap();
        assert_eq!(store.resolve_digest(&object(99, 20)).unwrap(), None);
        assert_eq!(store.resolve_backend_object(&digest(99)).unwrap(), None);
    }

    #[test]
    fn record_is_last_writer_wins_on_the_digest_key() {
        // Tripwire: re-recording a digest overwrites its object (idempotent
        // rebuild), never a second row that would make the forward resolve
        // ambiguous.
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let d = digest(1);
        store.record(&d, &object(2, 20)).unwrap();
        store.record(&d, &object(3, 32)).unwrap();
        assert_eq!(store.resolve_backend_object(&d).unwrap(), Some(object(3, 32)));
        // The superseded object no longer reverse-resolves to the digest.
        assert_eq!(store.resolve_digest(&object(2, 20)).unwrap(), None);
    }

    #[test]
    fn record_is_last_writer_wins_on_the_object_key() {
        // Tripwire: re-pointing one git object at a new digest evicts the prior
        // reverse row. Before `(git_format, git_bytes)` was a UNIQUE index the stale
        // row survived — two rows shared the object and `resolve_digest`'s `LIMIT 1`
        // could serve the retired digest.
        let store = SqliteCorrespondence::open(":memory:").unwrap();
        let object = object(2, 20);
        store.record(&digest(1), &object).unwrap();
        store.record(&digest(9), &object).unwrap();
        // The reverse resolves to the new digest, and the retired digest's forward
        // row was dropped on the object-key conflict rather than left dangling.
        assert_eq!(store.resolve_digest(&object).unwrap(), Some(digest(9)));
        assert_eq!(store.resolve_backend_object(&digest(1)).unwrap(), None);
    }

    #[test]
    fn legacy_sha1_and_sha256_rows_migrate_once_and_survive_reopens() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE git_correspondence (\
                     digest BLOB NOT NULL PRIMARY KEY,\
                     git_format INTEGER NOT NULL,\
                     git_bytes BLOB NOT NULL\
                 );\
                 CREATE UNIQUE INDEX git_correspondence_by_object \
                     ON git_correspondence (git_format, git_bytes);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO git_correspondence (digest, git_format, git_bytes) VALUES (?1, 1, ?2)",
                rusqlite::params![digest(1).as_bytes().as_slice(), object(2, 20).as_bytes()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO git_correspondence (digest, git_format, git_bytes) VALUES (?1, 2, ?2)",
                rusqlite::params![digest(3).as_bytes().as_slice(), object(4, 32).as_bytes()],
            )
            .unwrap();
        drop(connection);

        for attempt in 0..3 {
            let store = SqliteCorrespondence::open(path).unwrap();
            for (digest, object) in [(digest(1), object(2, 20)), (digest(3), object(4, 32))] {
                assert_eq!(
                    store.resolve_backend_object(&digest).unwrap(),
                    Some(object.clone()),
                    "forward lookup survives open {attempt}",
                );
                assert_eq!(
                    store.resolve_digest(&object).unwrap(),
                    Some(digest),
                    "reverse lookup survives open {attempt}",
                );
            }
        }

        let connection = Connection::open(path).unwrap();
        let legacy_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'git_correspondence')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let rows: i64 =
            connection.query_row("SELECT COUNT(*) FROM backend_correspondence", [], |row| row.get(0)).unwrap();
        assert!(!legacy_exists, "a successful migration drops the legacy table exactly once");
        assert_eq!(rows, 2, "reopening is a no-op and does not duplicate migrated rows");
    }

    #[test]
    fn a_failed_legacy_copy_rolls_back_without_dropping_the_source_table() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE backend_correspondence (\
                     digest BLOB NOT NULL PRIMARY KEY,\
                     backend_object BLOB NOT NULL UNIQUE CHECK(length(backend_object) = 99)\
                 );\
                 CREATE TABLE git_correspondence (\
                     digest BLOB NOT NULL PRIMARY KEY,\
                     git_format INTEGER NOT NULL,\
                     git_bytes BLOB NOT NULL\
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO git_correspondence (digest, git_format, git_bytes) VALUES (?1, 1, ?2)",
                rusqlite::params![digest(1).as_bytes().as_slice(), object(2, 20).as_bytes()],
            )
            .unwrap();
        drop(connection);

        assert!(SqliteCorrespondence::open(path).is_err(), "the incompatible target rejects the legacy copy");

        let connection = Connection::open(path).unwrap();
        let legacy_rows: i64 =
            connection.query_row("SELECT COUNT(*) FROM git_correspondence", [], |row| row.get(0)).unwrap();
        let generic_rows: i64 =
            connection.query_row("SELECT COUNT(*) FROM backend_correspondence", [], |row| row.get(0)).unwrap();
        assert_eq!(legacy_rows, 1, "the source row remains after rollback");
        assert_eq!(generic_rows, 0, "the failed copy leaves no partial generic row");
    }
}
