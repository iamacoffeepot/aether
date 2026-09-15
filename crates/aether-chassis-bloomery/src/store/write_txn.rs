//! How a transaction that is going to write begins.
//!
//! `Connection::transaction` begins `DEFERRED`: no lock is taken until the
//! first statement needs one. Every write path here reads before it writes —
//! `insert_commission` selects the id before inserting it, `write_revision`
//! loads the head before advancing it, the session pool reads its row before
//! marking the lease — so a deferred transaction is holding a read snapshot by
//! the time it asks for the write lock, and SQLite refuses *that* upgrade with
//! an immediate `SQLITE_BUSY`. The refusal deliberately skips the busy handler:
//! two connections each waiting to upgrade their own snapshot would deadlock,
//! so SQLite returns rather than waits. A connection opened through
//! `SqliteStore::open_with_busy_timeout` therefore reports "database is locked"
//! the instant the coordinator is mid-write, having waited none of the timeout
//! it was given (iamacoffeepot/aether#6078).
//!
//! `IMMEDIATE` takes the write lock at `BEGIN`, before any snapshot exists,
//! which is the one place the busy handler does apply — so a writer that raced
//! the coordinator waits its turn and then proceeds. [`holder::claim`] and
//! [`class::stamp`] already begin this way for the same reason; this is that
//! rule stated once for every write path in the journal.
//!
//! [`holder::claim`]: super::holder
//! [`class::stamp`]: super::class

use rusqlite::{Connection, Transaction, TransactionBehavior};

/// Begin a write transaction on `conn`.
///
/// # Errors
/// The `BEGIN IMMEDIATE` could not be issued — including a `SQLITE_BUSY` that
/// outlasted the connection's busy timeout.
pub(crate) fn begin(conn: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use rusqlite::Connection;

    use super::begin;

    /// Open a file-backed WAL connection whose busy handler is allowed to wait.
    fn connect(path: &str) -> Connection {
        let conn = Connection::open(path).expect("the scratch journal opens");
        conn.pragma_update(None, "journal_mode", "WAL").expect("WAL is selected");
        conn.busy_timeout(Duration::from_secs(10)).expect("the busy handler is given a budget");
        conn.execute("CREATE TABLE IF NOT EXISTS rows (id TEXT PRIMARY KEY)", []).expect("the fixture table exists");
        conn
    }

    /// Read a row, then write one — the shape every journal write path has.
    fn read_then_write(conn: &mut Connection, id: &str, immediate: bool) -> rusqlite::Result<()> {
        let txn = if immediate {
            begin(conn)?
        } else {
            conn.transaction()?
        };
        let _existing: Option<String> = txn.query_row("SELECT id FROM rows WHERE id = ?1", [id], |row| row.get(0)).ok();
        txn.execute("INSERT INTO rows (id) VALUES (?1)", [id])?;
        txn.commit()
    }

    /// Hold the write lock for `hold`, on a connection of its own.
    fn hold_the_write_lock(path: &str, id: &str, hold: Duration) -> thread::JoinHandle<()> {
        let (path, id) = (path.to_owned(), id.to_owned());
        thread::spawn(move || {
            let mut holder = connect(&path);
            let txn = begin(&mut holder).expect("the holder takes the write lock");
            txn.execute("INSERT INTO rows (id) VALUES (?1)", [&id]).expect("the holder writes");
            thread::sleep(hold);
            txn.commit().expect("the holder commits");
        })
    }

    /// Tripwire: the busy timeout is only reachable from `BEGIN IMMEDIATE`. A
    /// deferred read-then-write loses its upgrade outright while another
    /// connection holds the write lock, however long a budget it was given —
    /// which is why every write path in this store begins through
    /// [`begin`]. Revert `begin` to `Connection::transaction` and the
    /// immediate half of this test fails with "database is locked".
    #[test]
    fn a_write_that_raced_the_holder_waits_its_turn_only_when_it_began_immediate() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("journal.db");
        let path = path.to_str().expect("the scratch path is UTF-8");
        drop(connect(path));

        let holder = hold_the_write_lock(path, "holder-deferred", Duration::from_millis(400));
        thread::sleep(Duration::from_millis(100));
        let mut deferred = connect(path);
        let refused = read_then_write(&mut deferred, "deferred", false);
        assert!(refused.is_err(), "a deferred read-then-write is refused the upgrade rather than waiting: {refused:?}",);
        holder.join().expect("the holder thread finishes");

        let holder = hold_the_write_lock(path, "holder-immediate", Duration::from_millis(400));
        thread::sleep(Duration::from_millis(100));
        let mut immediate = connect(path);
        read_then_write(&mut immediate, "immediate", true).expect("an immediate write waits the holder out");
        holder.join().expect("the holder thread finishes");
    }
}
