//! The `SQLite`-backed runtime for [`StoreCapability`] (ADR-0149 §The boundary).
//!
//! A single [`rusqlite::Connection`] in WAL mode, owned by the capability's
//! dispatcher (single writer by construction — one actor, one connection). The
//! blocking boundary (`docs/guide/capability-anatomy.md`): each handler runs one
//! short, local `SQLite` transaction inline. These are provably short — a bounded
//! `INSERT` / `SELECT` against a local file with no network — so they do not go
//! through `dispatch_blocking`; the actor's serialized dispatch is the single
//! writer the WAL journal wants.

use super::StoreCapability;
use super::kinds::{
    AckOutbox, AckOutboxResult, AppendEvent, AppendEventResult, ClaimSeal, ClaimSealResult, DrainOutbox,
    DrainOutboxResult, EnqueueOutbox, EnqueueOutboxResult, OutboxEntry, ReleaseMembership, ReleaseMembershipResult,
    Supersede, SupersedeResult,
};
use aether_actor::runtime;
// The control-plane transact-mails the wasm control actor drives — `Commit` and
// the `ReplayJournal` family — are defined in `aether-bloomery` to avoid a
// package cycle (the actor lives there; host depends on it). Host imports them
// inward for its `StoreCapability` handlers (issue #3497).
use aether_bloomery::{
    Commit, CommitResult, JournalRecord, MembershipMutation, OutboxPayload, ReplayJournal, ReplayJournalResult,
};
use rusqlite::Connection;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// The outcome of an [`AppendEvent`]: either the event was journaled at a new
/// sequence, or its idempotency key was already recorded (inbox dedup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The event was appended at this journal sequence.
    Applied(u64),
    /// The idempotency key was already present — nothing was appended.
    Duplicate,
}

/// The outcome of a [`ClaimSeal`]: the whole membership set claimed, or the
/// first workpiece already held by an active bloom (the seal claimed nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealOutcome {
    /// Every workpiece was claimed — the seal is durable.
    Sealed,
    /// A workpiece was already active; the transaction rolled back.
    Conflict(String),
}

/// The outcome of a combined [`Commit`]: the whole decision journaled +
/// applied at a new sequence, the idempotency key already present (no-op), or
/// a claimed workpiece already held by an active bloom (the whole commit rolled
/// back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The event journaled and every membership/outbox effect applied, at this
    /// journal sequence.
    Applied(u64),
    /// The idempotency key was already journaled — nothing was applied.
    Duplicate,
    /// A claimed workpiece was already active; the whole transaction rolled back.
    Conflict(String),
}

/// The durable store the capability drives. One method per transact-mail kind;
/// each is one atomic `SQLite` transaction.
pub trait StoreBackend: Send {
    /// Apply a combined commit — journal the idempotency-keyed event, apply the
    /// membership releases then claims, and enqueue the outbox payloads — in
    /// **one** transaction (ADR-0149 §The control core). A duplicate key or a
    /// membership conflict applies nothing.
    fn commit(
        &mut self,
        idempotency_key: &str,
        event: &[u8],
        releases: &[MembershipMutation],
        claims: &[MembershipMutation],
        outbox: &[OutboxPayload],
    ) -> rusqlite::Result<CommitOutcome>;
    /// Append a journal event, deduplicated by `idempotency_key`.
    fn append_event(&mut self, idempotency_key: &str, event: &[u8]) -> rusqlite::Result<AppendOutcome>;
    /// Claim every workpiece for `bloom` under the active-membership uniqueness
    /// constraint, all-or-nothing.
    fn claim_seal(&mut self, bloom: &[u8], members: &[String]) -> rusqlite::Result<SealOutcome>;
    /// Atomically release `predecessor`'s memberships and claim `successor`'s
    /// members, in one transaction.
    fn supersede(&mut self, predecessor: &[u8], successor: &[u8], members: &[String]) -> rusqlite::Result<SealOutcome>;
    /// Release every active membership `bloom` holds; returns how many.
    fn release_membership(&mut self, bloom: &[u8]) -> rusqlite::Result<u32>;
    /// Enqueue an outbox entry; returns its sequence.
    fn enqueue_outbox(&mut self, topic: &str, payload: &[u8]) -> rusqlite::Result<u64>;
    /// Read every undelivered outbox entry, in sequence order.
    fn drain_outbox(&mut self) -> rusqlite::Result<Vec<OutboxEntry>>;
    /// Mark every outbox entry at or below `through_sequence` delivered; returns
    /// how many were newly acknowledged.
    fn ack_outbox(&mut self, through_sequence: u64) -> rusqlite::Result<u32>;
    /// Read the whole journal, in sequence order — the recovery replay source.
    fn replay_journal(&mut self) -> rusqlite::Result<Vec<JournalRecord>>;
}

/// A WAL-mode `SQLite` store. Opening runs the migrations idempotently, so
/// reopening the same file on restart resumes against the persisted journal.
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Open (or create) a store at `path`. `":memory:"` opens a private,
    /// non-durable in-memory database — the same code path, used by tests and
    /// the default unconfigured chassis.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // WAL gives a durable single-writer / many-reader journal; a `:memory:`
        // database silently ignores the pragma (it has one connection anyway).
        // `synchronous=NORMAL` is the WAL-appropriate durability point: a
        // committed transaction survives an application crash (`kill -9`).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(MIGRATIONS)?;
        Ok(Self { conn })
    }
}

/// The schema, applied idempotently on every open.
const MIGRATIONS: &str = "\
CREATE TABLE IF NOT EXISTS journal (
    sequence        INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL UNIQUE,
    event           BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS active_membership (
    workpiece TEXT PRIMARY KEY,
    bloom     BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS active_membership_by_bloom ON active_membership (bloom);
CREATE TABLE IF NOT EXISTS outbox (
    sequence  INTEGER PRIMARY KEY AUTOINCREMENT,
    topic     TEXT NOT NULL,
    payload   BLOB NOT NULL,
    delivered INTEGER NOT NULL DEFAULT 0
);
";

/// Is a rusqlite error a UNIQUE / PRIMARY KEY constraint violation? A seal that
/// hits one is a membership conflict, not a store failure.
fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

impl StoreBackend for SqliteStore {
    fn commit(
        &mut self,
        idempotency_key: &str,
        event: &[u8],
        releases: &[MembershipMutation],
        claims: &[MembershipMutation],
        outbox: &[OutboxPayload],
    ) -> rusqlite::Result<CommitOutcome> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "INSERT OR IGNORE INTO journal (idempotency_key, event) VALUES (?1, ?2)",
            rusqlite::params![idempotency_key, event],
        )?;
        if changed == 0 {
            // The key was already journaled — the whole commit is a no-op. The
            // transaction rolls back on drop, so no membership/outbox row applies
            // twice on a replayed key (the durable inbox-dedup backstop).
            return Ok(CommitOutcome::Duplicate);
        }
        // A rowid is a non-negative i64; the fallback never triggers.
        let sequence = u64::try_from(tx.last_insert_rowid()).unwrap_or_default();
        // Releases before claims: a superseding successor reclaims a workpiece
        // its predecessor freed in this same transaction (ADR-0149 §The bloom).
        for release in releases {
            tx.execute(
                "DELETE FROM active_membership WHERE workpiece = ?1 AND bloom = ?2",
                rusqlite::params![release.workpiece, release.bloom],
            )?;
        }
        for claim in claims {
            let insert = tx.execute(
                "INSERT INTO active_membership (workpiece, bloom) VALUES (?1, ?2)",
                rusqlite::params![claim.workpiece, claim.bloom],
            );
            match insert {
                Ok(_) => {}
                Err(error) if is_constraint_violation(&error) => {
                    // The transaction rolls back on drop — the journal append and
                    // every release roll back too, so a conflicted commit applies
                    // nothing (ADR-0149 all-or-nothing admission).
                    return Ok(CommitOutcome::Conflict(claim.workpiece.clone()));
                }
                Err(error) => return Err(error),
            }
        }
        for entry in outbox {
            tx.execute(
                "INSERT INTO outbox (topic, payload) VALUES (?1, ?2)",
                rusqlite::params![entry.topic, entry.payload],
            )?;
        }
        tx.commit()?;
        Ok(CommitOutcome::Applied(sequence))
    }

    fn append_event(&mut self, idempotency_key: &str, event: &[u8]) -> rusqlite::Result<AppendOutcome> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO journal (idempotency_key, event) VALUES (?1, ?2)",
            rusqlite::params![idempotency_key, event],
        )?;
        if changed == 0 {
            Ok(AppendOutcome::Duplicate)
        } else {
            // A rowid is a non-negative i64; the fallback never triggers.
            Ok(AppendOutcome::Applied(u64::try_from(self.conn.last_insert_rowid()).unwrap_or_default()))
        }
    }

    fn claim_seal(&mut self, bloom: &[u8], members: &[String]) -> rusqlite::Result<SealOutcome> {
        let tx = self.conn.transaction()?;
        for workpiece in members {
            let insert = tx.execute(
                "INSERT INTO active_membership (workpiece, bloom) VALUES (?1, ?2)",
                rusqlite::params![workpiece, bloom],
            );
            match insert {
                Ok(_) => {}
                Err(error) if is_constraint_violation(&error) => {
                    // The transaction rolls back on drop — the whole seal claims
                    // nothing (ADR-0149 all-or-nothing admission).
                    return Ok(SealOutcome::Conflict(workpiece.clone()));
                }
                Err(error) => return Err(error),
            }
        }
        tx.commit()?;
        Ok(SealOutcome::Sealed)
    }

    fn supersede(&mut self, predecessor: &[u8], successor: &[u8], members: &[String]) -> rusqlite::Result<SealOutcome> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM active_membership WHERE bloom = ?1", rusqlite::params![predecessor])?;
        for workpiece in members {
            let insert = tx.execute(
                "INSERT INTO active_membership (workpiece, bloom) VALUES (?1, ?2)",
                rusqlite::params![workpiece, successor],
            );
            match insert {
                Ok(_) => {}
                Err(error) if is_constraint_violation(&error) => {
                    // Rolls back the DELETE too — the predecessor keeps its
                    // claims and the successor claims nothing.
                    return Ok(SealOutcome::Conflict(workpiece.clone()));
                }
                Err(error) => return Err(error),
            }
        }
        tx.commit()?;
        Ok(SealOutcome::Sealed)
    }

    fn release_membership(&mut self, bloom: &[u8]) -> rusqlite::Result<u32> {
        let released = self.conn.execute("DELETE FROM active_membership WHERE bloom = ?1", rusqlite::params![bloom])?;
        Ok(u32::try_from(released).unwrap_or(u32::MAX))
    }

    fn enqueue_outbox(&mut self, topic: &str, payload: &[u8]) -> rusqlite::Result<u64> {
        self.conn.execute("INSERT INTO outbox (topic, payload) VALUES (?1, ?2)", rusqlite::params![topic, payload])?;
        Ok(u64::try_from(self.conn.last_insert_rowid()).unwrap_or_default())
    }

    fn drain_outbox(&mut self) -> rusqlite::Result<Vec<OutboxEntry>> {
        let mut stmt =
            self.conn.prepare("SELECT sequence, topic, payload FROM outbox WHERE delivered = 0 ORDER BY sequence")?;
        let rows = stmt.query_map([], |row| {
            Ok(OutboxEntry {
                sequence: u64::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
                topic: row.get(1)?,
                payload: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    fn ack_outbox(&mut self, through_sequence: u64) -> rusqlite::Result<u32> {
        let acked = self.conn.execute(
            "UPDATE outbox SET delivered = 1 WHERE sequence <= ?1 AND delivered = 0",
            rusqlite::params![i64::try_from(through_sequence).unwrap_or(i64::MAX)],
        )?;
        Ok(u32::try_from(acked).unwrap_or(u32::MAX))
    }

    fn replay_journal(&mut self) -> rusqlite::Result<Vec<JournalRecord>> {
        let mut stmt = self.conn.prepare("SELECT sequence, idempotency_key, event FROM journal ORDER BY sequence")?;
        let rows = stmt.query_map([], |row| {
            Ok(JournalRecord {
                sequence: u64::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
                idempotency_key: row.get(1)?,
                event: row.get(2)?,
            })
        })?;
        rows.collect()
    }
}

/// Runtime state for [`StoreCapability`]: the one durable backend the
/// dispatcher owns.
pub struct StoreCapabilityState {
    backend: Box<dyn StoreBackend>,
}

impl StoreCapabilityState {
    /// Build state over an explicit backend — the seam the handler tests drive.
    #[must_use]
    pub fn new(backend: Box<dyn StoreBackend>) -> Self {
        Self { backend }
    }
}

#[runtime]
impl NativeActor for StoreCapability {
    type State = StoreCapabilityState;
    type Config = super::StoreConfig;

    const NAMESPACE: &'static str = "aether.store";

    fn init(config: super::StoreConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<StoreCapabilityState, BootError> {
        let store = SqliteStore::open(&config.path).map_err(|error| BootError::Other(Box::new(error)))?;
        tracing::info!(target: "aether_bloomery_host::store", path = %config.path, "store opened (WAL)");
        Ok(StoreCapabilityState { backend: Box::new(store) })
    }

    #[handler::single]
    fn on_commit(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Commit) -> CommitResult {
        let Commit { idempotency_key, event, releases, claims, outbox } = mail;
        match state.backend.commit(&idempotency_key, &event, &releases, &claims, &outbox) {
            Ok(CommitOutcome::Applied(sequence)) => CommitResult::Applied { idempotency_key, sequence },
            Ok(CommitOutcome::Duplicate) => CommitResult::Duplicate { idempotency_key },
            Ok(CommitOutcome::Conflict(workpiece)) => CommitResult::Conflict { idempotency_key, workpiece },
            Err(error) => CommitResult::Err { idempotency_key, error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_append_event(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AppendEvent) -> AppendEventResult {
        let AppendEvent { idempotency_key, event } = mail;
        match state.backend.append_event(&idempotency_key, &event) {
            Ok(AppendOutcome::Applied(sequence)) => AppendEventResult::Applied { sequence },
            Ok(AppendOutcome::Duplicate) => AppendEventResult::Duplicate,
            Err(error) => AppendEventResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_claim_seal(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ClaimSeal) -> ClaimSealResult {
        let ClaimSeal { bloom, members } = mail;
        match state.backend.claim_seal(&bloom, &members) {
            Ok(SealOutcome::Sealed) => ClaimSealResult::Sealed,
            Ok(SealOutcome::Conflict(workpiece)) => ClaimSealResult::Conflict { workpiece },
            Err(error) => ClaimSealResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_supersede(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Supersede) -> SupersedeResult {
        let Supersede { predecessor, successor, members } = mail;
        match state.backend.supersede(&predecessor, &successor, &members) {
            Ok(SealOutcome::Sealed) => SupersedeResult::Sealed,
            Ok(SealOutcome::Conflict(workpiece)) => SupersedeResult::Conflict { workpiece },
            Err(error) => SupersedeResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_release_membership(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ReleaseMembership,
    ) -> ReleaseMembershipResult {
        let ReleaseMembership { bloom } = mail;
        match state.backend.release_membership(&bloom) {
            Ok(released) => ReleaseMembershipResult::Ok { released },
            Err(error) => ReleaseMembershipResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_enqueue_outbox(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: EnqueueOutbox,
    ) -> EnqueueOutboxResult {
        let EnqueueOutbox { topic, payload } = mail;
        match state.backend.enqueue_outbox(&topic, &payload) {
            Ok(sequence) => EnqueueOutboxResult::Ok { sequence },
            Err(error) => EnqueueOutboxResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_drain_outbox(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrainOutbox) -> DrainOutboxResult {
        match state.backend.drain_outbox() {
            Ok(entries) => DrainOutboxResult::Ok { entries },
            Err(error) => DrainOutboxResult::Err { error: error.to_string() },
        }
    }

    // The `#[handler::single]` contract requires the mail by value; `AckOutbox`
    // is a single-`Copy`-field struct, so clippy sees a by-ref opportunity the
    // macro signature cannot take.
    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_ack_outbox(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AckOutbox) -> AckOutboxResult {
        match state.backend.ack_outbox(mail.through_sequence) {
            Ok(acked) => AckOutboxResult::Ok { acked },
            Err(error) => AckOutboxResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_replay_journal(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: ReplayJournal,
    ) -> ReplayJournalResult {
        match state.backend.replay_journal() {
            Ok(records) => ReplayJournalResult::Ok { records },
            Err(error) => ReplayJournalResult::Err { error: error.to_string() },
        }
    }
}
