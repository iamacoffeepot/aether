//! The class a journal records — live operation, or a benchmark trial store
//! (ADR-0184).
//!
//! A benchmark bloom replays landed history against the fixture repository
//! (#4732, promoted to a runtime mode in #4871 piece 1), so it exercises the
//! shipped coordinator against a world that never existed. ADR-0184 makes the
//! *store* the thing that tells those rows from real ones — "distinguishable by
//! their trial store" — which is why the class lives here rather than on each
//! journal row: one coordinator generation writes one journal, so a row's class
//! is a fact about the file, and a per-row copy could only ever disagree with
//! it. No persisted row shape changes, and no ADR-0187 upcast is owed.
//!
//! The stamp is one row in a table this module creates and reads *before* the
//! schema migrations, exactly as the holder claim does and for the same reason:
//! a refused open must not migrate and must not write. Taking it is one
//! `BEGIN IMMEDIATE` transaction so a second process decides against a
//! committed stamp rather than a stale snapshot.
//!
//! Three states, and the refusals they produce:
//!
//! - **Stamped, and it agrees** — the ordinary reopen.
//! - **Stamped, and it disagrees** — a trial coordinator was pointed at the
//!   estate's journal, or a live one at a calibration host's. Refused by name.
//!   This is the guard that makes "benchmark blooms never touch the live
//!   repository or its refs" true of the journal as well as of the refs.
//! - **Unstamped** — every journal written before this gate existed. An empty
//!   one takes the class it is opened with; one that already holds rows is live
//!   history by construction, so it takes `live` and refuses a trial open.

use std::error::Error as StdError;
use std::fmt;

use aether_bloomery::StoreClass;
use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior};

use super::runtime::now_unix_millis;

/// The stamp table, created before the schema migrations and independent of
/// them: a store that predates this gate gains the table on its next open, and
/// a store the gate refuses is left exactly as it was found.
const CLASS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS journal_class (\n\
    id INTEGER PRIMARY KEY CHECK (id = 0),\n\
    class TEXT NOT NULL,\n\
    stamped_unix_millis INTEGER NOT NULL\n\
);";

/// Why a journal could not be opened under the class the coordinator resolved.
#[derive(Debug)]
pub enum StoreClassError {
    /// The journal is stamped with a different class than this coordinator
    /// resolved — the misdirected-benchmark refusal.
    Mismatch {
        /// The journal that was refused.
        path: String,
        /// The class the journal records.
        recorded: StoreClass,
        /// The class this coordinator resolved from its configuration.
        requested: StoreClass,
    },
    /// The journal carries no stamp but already holds rows, so it is live
    /// history: a benchmark run may not append to it.
    LiveHistory {
        /// The journal that was refused.
        path: String,
        /// How many journal rows made it live history.
        rows: u64,
    },
    /// The recorded stamp is not a class name. Refused rather than read as
    /// `live`, because guessing is how a benchmark's rows would enter the
    /// estate's measurement.
    Unreadable {
        /// The journal that was refused.
        path: String,
        /// The stamp as it was found.
        recorded: String,
    },
    /// The stamp could not be read or written.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for StoreClassError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch { path, recorded, requested } => write!(
                f,
                "journal {path} records {} rows and this coordinator resolved {}; refusing to mix benchmark with \
                 live measurement in one store (ADR-0184). Point AETHER_STORE_PATH at the store for this mode, or \
                 correct AETHER_GITHUB_BACKEND / AETHER_STORE_CLASS",
                recorded.as_str(),
                requested.as_str(),
            ),
            Self::LiveHistory { path, rows } => write!(
                f,
                "journal {path} carries no class stamp and already holds {rows} row(s), so it is live history; \
                 refusing to write benchmark rows into it. Point AETHER_STORE_PATH at a store the calibration \
                 host owns",
            ),
            Self::Unreadable { path, recorded } => {
                write!(f, "journal {path} records the unknown class {recorded:?}; expected live or trial")
            }
            Self::Sqlite(error) => write!(f, "journal class stamp: {error}"),
        }
    }
}

impl StdError for StoreClassError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreClassError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Stamp `path` with `requested`, or refuse because the journal belongs to the
/// other world.
///
/// Runs before the schema migrations, so a refusal leaves the journal exactly
/// as it was found.
///
/// # Errors
/// The journal records a different class, is unstamped live history opened as a
/// trial store, records a name outside the vocabulary, or the stamp could not
/// be read or written.
pub(super) fn stamp(conn: &Connection, path: &str, requested: StoreClass) -> Result<(), StoreClassError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(CLASS_TABLE)?;
    match read_stamp(&tx)? {
        Some(recorded) => {
            let recorded = StoreClass::parse(&recorded)
                .ok_or_else(|| StoreClassError::Unreadable { path: path.to_owned(), recorded })?;
            if recorded != requested {
                return Err(StoreClassError::Mismatch { path: path.to_owned(), recorded, requested });
            }
        }
        None => {
            let rows = journal_rows(&tx)?;
            if requested == StoreClass::Trial && rows > 0 {
                return Err(StoreClassError::LiveHistory { path: path.to_owned(), rows });
            }
            write_stamp(&tx, requested)?;
            tracing::info!(
                target: "aether_chassis_bloomery::store",
                path,
                class = requested.as_str(),
                rows,
                "journal class stamped",
            );
        }
    }
    tx.commit()?;
    Ok(())
}

/// The class `conn`'s journal records, for a read that never claimed it.
///
/// An unstamped journal reads as [`StoreClass::Live`]: every store written
/// before this gate existed is the estate's own, and a trial store is stamped
/// on the open that creates it.
///
/// # Errors
/// The stamp could not be read, or records a name outside the vocabulary.
pub(super) fn recorded(conn: &Connection) -> Result<StoreClass, StoreClassError> {
    if !stamp_table_exists(conn)? {
        return Ok(StoreClass::Live);
    }
    match read_stamp(conn)? {
        None => Ok(StoreClass::Live),
        Some(recorded) => StoreClass::parse(&recorded)
            .ok_or_else(|| StoreClassError::Unreadable { path: conn.path().unwrap_or_default().to_owned(), recorded }),
    }
}

fn stamp_table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'journal_class'", [], |row| {
        row.get::<_, i64>(0)
    })
    .map(|found| found > 0)
}

fn read_stamp(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT class FROM journal_class WHERE id = 0", [], |row| row.get(0)).optional()
}

fn write_stamp(conn: &Connection, class: StoreClass) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO journal_class (id, class, stamped_unix_millis) VALUES (0, ?1, ?2)",
        rusqlite::params![class.as_str(), now_unix_millis()],
    )?;
    Ok(())
}

/// How many rows the journal already holds, or `0` when it has no journal table
/// at all — the migrations have not run yet on a store this gate is deciding.
fn journal_rows(conn: &Connection) -> rusqlite::Result<u64> {
    let present: i64 =
        conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'journal'", [], |row| {
            row.get(0)
        })?;
    if present == 0 {
        return Ok(0);
    }
    let rows: i64 = conn.query_row("SELECT count(*) FROM journal", [], |row| row.get(0))?;
    Ok(u64::try_from(rows).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use aether_bloomery::StoreClass;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::{StoreClassError, recorded, stamp};
    use crate::store::{JournalOpenError, SqliteStore};

    fn journal_path(dir: &TempDir) -> String {
        dir.path().join("bloomery.db").to_str().expect("a temp path is utf-8").to_owned()
    }

    fn table_exists(path: &str, table: &str) -> bool {
        Connection::open(path)
            .expect("the journal opens")
            .query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1", [table], |row| {
                row.get::<_, i64>(0)
            })
            .expect("sqlite_master is readable")
            > 0
    }

    #[test]
    fn a_live_journal_refuses_a_trial_open_without_migrating() {
        // The bug this catches: a calibration host pointed at the estate's
        // journal (a copied AETHER_STORE_PATH, an unset one resolving to the
        // deployed file) appending benchmark rows to real history, where
        // nothing downstream could ever separate them again. The refusal has to
        // happen before the migrations, because a gate that refuses *after*
        // writing the schema has already touched the file it exists to protect.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = journal_path(&dir);
        let seeded = Connection::open(&path).expect("the journal opens");
        stamp(&seeded, &path, StoreClass::Live).expect("the journal is stamped live");
        drop(seeded);

        let refusal =
            SqliteStore::open_as_holder(&path, StoreClass::Trial).err().expect("a live journal refuses a trial open");
        let JournalOpenError::Class(StoreClassError::Mismatch { recorded, requested, path: named }) = &refusal else {
            panic!("the refusal must name both classes: {refusal}");
        };
        assert_eq!(*recorded, StoreClass::Live);
        assert_eq!(*requested, StoreClass::Trial);
        assert_eq!(named, &path, "the refusal names the journal");
        assert!(!table_exists(&path, "journal"), "a refused open must not run the migrations");
    }

    #[test]
    fn a_trial_journal_refuses_a_live_open() {
        // The other direction, and the one that keeps the estate's measurement
        // honest: a coordinator resuming a calibration host's journal as if it
        // were its own would fold benchmark blooms into live operation.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = journal_path(&dir);
        let opened = SqliteStore::open_as_holder(&path, StoreClass::Trial).expect("a fresh journal opens trial");
        drop(opened);

        let refusal =
            SqliteStore::open_as_holder(&path, StoreClass::Live).err().expect("a trial journal refuses a live open");
        assert!(
            matches!(refusal, JournalOpenError::Class(StoreClassError::Mismatch { recorded: StoreClass::Trial, .. })),
            "{refusal}",
        );
    }

    #[test]
    fn an_unstamped_journal_with_rows_is_live_history() {
        // Every journal written before this gate carries no stamp. An empty one
        // may become either; one that already holds rows is the estate's, and
        // reading it as "unclaimed, so whatever you asked for" is how a
        // deployed journal would silently become a trial store on the first
        // mistyped backend.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = journal_path(&dir);
        let conn = Connection::open(&path).expect("the journal opens");
        conn.execute_batch(
            "CREATE TABLE journal (sequence INTEGER PRIMARY KEY AUTOINCREMENT, idempotency_key TEXT NOT NULL UNIQUE, \
             event BLOB NOT NULL); INSERT INTO journal (idempotency_key, event) VALUES ('k', x'00');",
        )
        .expect("a journal row is seeded");

        let refusal = stamp(&conn, &path, StoreClass::Trial).expect_err("live history refuses a trial open");
        assert!(matches!(refusal, StoreClassError::LiveHistory { rows: 1, .. }), "{refusal}");

        stamp(&conn, &path, StoreClass::Live).expect("the same journal opens live");
        assert_eq!(recorded(&conn).expect("the stamp reads back"), StoreClass::Live);
    }

    #[test]
    fn an_unstamped_journal_reads_as_live() {
        // The reader's default has to be `live`, not "unknown": the calibration
        // ledger over a journal that predates the stamp is a ledger over real
        // operation, and every store on disk today is one.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = journal_path(&dir);
        let conn = Connection::open(&path).expect("the journal opens");
        assert!(!table_exists(&path, "journal_class"), "a read must not create the stamp table");
        assert_eq!(recorded(&conn).expect("an unstamped journal reads"), StoreClass::Live);
    }
}
