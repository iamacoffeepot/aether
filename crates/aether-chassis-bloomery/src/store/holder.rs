//! The advisory holder claim one coordinator generation takes on a journal.
//!
//! `SQLite` in WAL mode tolerates two writers mechanically, but the
//! coordinator's semantics do not: two generations folding the same journal
//! decide the same events twice and race each other's outbox drain. Nothing
//! stopped them — the open path ran the migrations unconditionally and never
//! asked whether anyone else was already here, so a supervisor restart that
//! overlapped the old process (or an operator starting a second coordinator by
//! hand) silently produced two writers.
//!
//! The claim is one row in the journal itself, in a table this module creates
//! and reads *before* the schema migrations run: a refused open must not
//! migrate and must not write, so the guard cannot sit behind a migration step.
//!
//! A bare pid is not an identity — the kernel recycles them — so the row also
//! carries the machine's boot token, which makes every claim written before a
//! reboot stale on sight. Within one boot the pid is the whole liveness
//! question, and the answer is read from the process table: `/proc/<pid>/stat`
//! where the host has it (a zombie is already gone), otherwise `kill -0`. The
//! residual is a pid recycled within a single boot, which reads as a live
//! holder and refuses the open; the refusal names the pid and the journal path
//! so an operator can see what it is being told to wait for.

use std::error::Error as StdError;
use std::fs;
use std::process::{Command, Stdio};
use std::{fmt, process};

use rusqlite::{Connection, OptionalExtension as _};

use super::runtime::now_unix_millis;

/// The claim table, created before the schema migrations and independent of
/// them: a store that predates this guard gains the table on its next open,
/// and a store refused by the guard is left exactly as it was found.
const HOLDER_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS journal_holder (\n\
    id INTEGER PRIMARY KEY CHECK (id = 0),\n\
    pid INTEGER NOT NULL,\n\
    boot_token TEXT NOT NULL,\n\
    claimed_unix_millis INTEGER NOT NULL\n\
);";

/// The Linux file whose contents change on every boot. Absent elsewhere, in
/// which case the token is empty on both sides of the comparison and carries
/// no signal — the pid probe answers alone.
const BOOT_TOKEN_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// Why a journal could not be opened as its holder.
#[derive(Debug)]
pub enum JournalHolderError {
    /// Another coordinator process holds this journal and is still running.
    Held {
        /// The journal the live holder claimed.
        path: String,
        /// The holder's process id, as it recorded it.
        pid: u32,
        /// When the holder took the claim.
        claimed_unix_millis: u64,
    },
    /// The claim could not be read or written.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for JournalHolderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Held { path, pid, claimed_unix_millis } => write!(
                f,
                "journal {path} is already held by a live coordinator (pid {pid}, claimed at {claimed_unix_millis} \
                 unix millis); refusing to run a second generation against one journal",
            ),
            Self::Sqlite(error) => write!(f, "journal holder claim: {error}"),
        }
    }
}

impl StdError for JournalHolderError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Held { .. } => None,
            Self::Sqlite(error) => Some(error),
        }
    }
}

impl From<rusqlite::Error> for JournalHolderError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// One recorded claim.
struct Claim {
    pid: u32,
    boot_token: String,
    claimed_unix_millis: u64,
}

impl Claim {
    /// Whether the process this claim names is still running *and* is still
    /// the process that took the claim. A claim from before the last reboot is
    /// stale whatever the pid table says.
    fn holder_is_live(&self) -> bool {
        self.boot_token == boot_token() && pid_is_live(self.pid)
    }
}

/// Claim `path` for this process, or refuse because someone else holds it.
///
/// Runs before the schema migrations: a refusal leaves the database exactly as
/// it was found, which is what makes the guard safe to put in front of a
/// migration that is otherwise unconditional.
pub(super) fn claim(conn: &Connection, path: &str) -> Result<(), JournalHolderError> {
    conn.execute_batch(HOLDER_TABLE)?;
    if let Some(recorded) = read_claim(conn)? {
        // This process reclaiming its own journal is a re-open, not an
        // overlap: the reactors that open their own connections, and a boot
        // that follows a torn-down one inside a single test process, are both
        // this case.
        if recorded.pid != process::id() && recorded.holder_is_live() {
            return Err(JournalHolderError::Held {
                path: path.to_owned(),
                pid: recorded.pid,
                claimed_unix_millis: recorded.claimed_unix_millis,
            });
        }
        tracing::warn!(
            target: "aether_chassis_bloomery::store",
            path,
            held_by = recorded.pid,
            claimed_unix_millis = recorded.claimed_unix_millis,
            "journal holder claim is stale; taking it over",
        );
    }
    write_claim(conn, process::id())?;
    Ok(())
}

/// Drop this process's claim on a clean shutdown.
///
/// Best-effort by design: a coordinator that dies without releasing leaves a
/// claim the staleness rule retires on the next open, so the release is a
/// courtesy that shortens the log, never the thing correctness rests on. A
/// claim another process has since taken over is left alone.
pub(super) fn release(conn: &Connection) {
    let _ = conn.execute("DELETE FROM journal_holder WHERE id = 0 AND pid = ?1", rusqlite::params![process::id()]);
}

/// Record `pid` as the holder, replacing whatever was there.
fn write_claim(conn: &Connection, pid: u32) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO journal_holder (id, pid, boot_token, claimed_unix_millis) VALUES (0, ?1, ?2, ?3) \
         ON CONFLICT(id) DO UPDATE SET pid = excluded.pid, boot_token = excluded.boot_token, \
         claimed_unix_millis = excluded.claimed_unix_millis",
        rusqlite::params![pid, boot_token(), now_unix_millis()],
    )?;
    Ok(())
}

fn read_claim(conn: &Connection) -> rusqlite::Result<Option<Claim>> {
    conn.query_row("SELECT pid, boot_token, claimed_unix_millis FROM journal_holder WHERE id = 0", [], |row| {
        Ok(Claim {
            pid: row.get::<_, i64>(0)?.try_into().unwrap_or(u32::MAX),
            boot_token: row.get(1)?,
            claimed_unix_millis: row.get::<_, i64>(2)?.try_into().unwrap_or(0),
        })
    })
    .optional()
}

/// The machine's boot token, or an empty string on a host that has none.
fn boot_token() -> String {
    fs::read_to_string(BOOT_TOKEN_PATH).map(|body| body.trim().to_owned()).unwrap_or_default()
}

/// Whether `pid` currently names a running process.
///
/// `/proc/<pid>/stat` is the reading on the platform the coordinator deploys
/// to, and it is the one that gets zombies right: a killed-but-unreaped holder
/// is gone, and treating it as live would refuse the restart that reaped it.
/// A host without `/proc` falls back to `kill -0`, which answers pid existence
/// only — it cannot see through a recycled pid, and it reads another user's
/// process as absent.
fn pid_is_live(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat.rsplit_once(')').and_then(|(_, after)| after.split_whitespace().next()) != Some("Z"),
        Err(_) if fs::metadata("/proc/self").is_ok() => false,
        Err(_) => signal_zero(pid),
    }
}

fn signal_zero(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::{HOLDER_TABLE, JournalHolderError, claim, read_claim, release, write_claim};
    use crate::store::SqliteStore;

    /// A journal file holding nothing but a claim on `pid` — the state a
    /// second coordinator finds when the first one is already running, with
    /// none of the schema a migration would have written.
    fn journal_claimed_by(dir: &TempDir, pid: u32) -> String {
        let path = dir.path().join("bloomery.db").to_str().expect("a temp path is utf-8").to_owned();
        let conn = Connection::open(&path).expect("the journal opens");
        conn.execute_batch(HOLDER_TABLE).expect("the claim table is created");
        write_claim(&conn, pid).expect("the claim is written");
        path
    }

    /// A live process this test owns, standing in for the coordinator
    /// generation that is still running.
    fn other_process() -> Child {
        Command::new("sleep")
            .arg("120")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("a sleep child forks")
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
    fn a_live_holder_refuses_the_second_open_without_migrating() {
        // The bug this catches: the open path migrated unconditionally, so a
        // second coordinator generation wrote to a journal another live one
        // was already folding. The refusal has to name the holder, and it has
        // to happen before the migrations — a guard that refuses *after*
        // writing the schema has already let the second generation touch the
        // file it was supposed to keep it out of.
        let mut holder = other_process();
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = journal_claimed_by(&dir, holder.id());

        let refusal = SqliteStore::open_as_holder(&path).err().expect("a live holder refuses the second open");

        let JournalHolderError::Held { pid, path: named, .. } = &refusal else {
            panic!("the refusal must name the holder, not fault: {refusal}");
        };
        assert_eq!(*pid, holder.id(), "the refusal names the pid holding the journal");
        assert_eq!(named, &path, "the refusal names the journal path");
        assert!(!table_exists(&path, "journal"), "a refused open must not run the migrations");

        holder.kill().expect("the stand-in holder is killed");
        holder.wait().expect("the stand-in holder is reaped");
    }

    #[test]
    fn a_dead_holders_claim_is_taken_over() {
        // The other half of the guard: a coordinator that crashed without
        // releasing must not lock its own journal against the restart. Only a
        // *live* holder refuses.
        let mut holder = other_process();
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = journal_claimed_by(&dir, holder.id());
        holder.kill().expect("the stand-in holder is killed");
        holder.wait().expect("the stand-in holder is reaped");

        let store = SqliteStore::open_as_holder(&path).expect("a stale claim is taken over");
        drop(store);

        assert!(table_exists(&path, "journal"), "the takeover ran the migrations it was gating");
    }

    #[test]
    fn a_clean_shutdown_releases_the_claim() {
        // The plausible bug: releasing on drop from any store handle, or not
        // at all. A released journal must carry no claim row, and a handle
        // that never claimed must not delete the holder's row out from under
        // it.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("bloomery.db").to_str().expect("a temp path is utf-8").to_owned();

        drop(SqliteStore::open_as_holder(&path).expect("an unclaimed journal is claimed"));
        let conn = Connection::open(&path).expect("the journal opens");
        assert!(read_claim(&conn).expect("the claim table is readable").is_none(), "a clean shutdown releases");

        claim(&conn, &path).expect("the claim is retaken");
        drop(SqliteStore::open(&path).expect("a plain open does not claim"));
        assert!(
            read_claim(&conn).expect("the claim table is readable").is_some(),
            "a non-holding handle must not release someone else's claim",
        );
        release(&conn);
    }
}
