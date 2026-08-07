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
    DrainOutboxResult, EnqueueOutbox, EnqueueOutboxResult, OutboxEntry, RecordDispatchDescription,
    RecordDispatchDescriptionResult, ReleaseMembership, ReleaseMembershipResult, Supersede, SupersedeResult,
};
use aether_actor::runtime;
// The control-plane transact-mails the wasm control actor drives — `Commit` and
// the `ReplayJournal` family — are defined in `aether-bloomery` to avoid a
// package cycle (the actor lives there; host depends on it). Host imports them
// inward for its `StoreCapability` handlers (issue #3497).
use aether_bloomery::{
    Commit, CommitResult, JournalRecord, MembershipMutation, OutboxPayload, ReplayJournal, ReplayJournalResult,
};
use std::time::Duration;

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

/// One outstanding work order the host dispatched and is waiting on evidence
/// for (ADR-0149 migration step 2, evidence intake — issue #3502). The
/// host-side dispatch-record that links a dispatched worker's idempotency
/// `nonce` back to the reducer context the returning evidence needs, so the
/// portable core [`aether_bloomery::WorkOrder`] stays `{ transformation, nonce }`
/// and never carries orchestration state. Persisted (not in-memory) because
/// evidence returns after an arbitrary delay — a worker run takes minutes — so
/// the order must survive a host restart to stay matchable, and consumed on
/// accept so a replayed nonce refuses.
///
/// The digest-typed columns (`bloom`, `scope_revision`, `candidate`,
/// `displayed_digest`) are the raw digest bytes, matching the opaque-bytes
/// convention the `bloom` axis of [`MembershipMutation`] already uses; the
/// native intake reconstructs the typed values from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutstandingOrder {
    /// The dispatched worker's idempotency nonce — the registry key the
    /// returning evidence is matched by.
    pub nonce: String,
    /// The bloom the resolved candidate integrates into (its `BloomId` digest
    /// bytes).
    pub bloom: Vec<u8>,
    /// The member workpiece this order resolves.
    pub workpiece: String,
    /// The scope revision the candidate was integrated against (digest bytes).
    pub scope_revision: Vec<u8>,
    /// The exact candidate digest the evidence must bind to (digest bytes).
    pub candidate: Vec<u8>,
    /// The digest Bloomery displayed for this order — the evidence's bound
    /// digest must equal it (digest bytes).
    pub displayed_digest: Vec<u8>,
    /// The line stage this order dispatched (a [`StageId`](aether_bloomery::StageId)
    /// as its canonical `aether_data::wire` bytes, #3505). The intake routes the
    /// returning result by stage: a non-terminal per-member stage admits as a
    /// `Fact::AttemptCompleted` that advances the member's cursor, the terminal
    /// `Review` as a `Fact::Integrate`, and a parked outcome as a `Question`.
    pub stage: Vec<u8>,
    /// The dispatched [`Transformation`](aether_bloomery::Transformation) as its
    /// canonical `aether_data::wire` bytes, following the `stage` column's
    /// convention. Persisted because a parked attempt is re-dispatched by
    /// *replaying* it (#3664), and the transformation's `checkout` is reducer-only
    /// state (`spec.base()`, or the cursor's candidate) that no other column
    /// carries — so re-deriving it host-side is not possible.
    pub transformation: Vec<u8>,
}

/// The outcome of recording an [`OutstandingOrder`]: written, or its nonce was
/// already outstanding (idempotent — nothing changed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The order was recorded at its nonce.
    Recorded,
    /// The nonce was already outstanding — nothing was written.
    Duplicate,
}

/// One row of the per-bloom study index (issue #3523): a graded attempt's
/// (`bloom`, `attempt_digest`) key and the content-store digest of the
/// `StudyRecord` artifact it resolves to. A *rebuildable projection* over the
/// artifact bytes — the study intake writes it on accept and the rebuild path
/// reconstructs it from the `aether.artifacts` store — so it is never a second
/// source of truth (ADR-0149: "the journal plus the content-addressed artifact
/// bytes are the only truth"). The digest-typed columns are raw bytes, matching
/// the [`OutstandingOrder`] convention; `study_artifact` is the content store's
/// hex digest string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudyRow {
    /// The sealed bloom the graded attempt belongs to (its `BloomId` digest
    /// bytes).
    pub bloom: Vec<u8>,
    /// The exact attempt digest the study record grades (digest bytes).
    pub attempt_digest: Vec<u8>,
    /// The content-store digest of the `StudyRecord` artifact.
    pub study_artifact: String,
}

/// The durable store the capability drives. One method per transact-mail kind;
/// each is one atomic `SQLite` transaction.
///
/// Outbox `topic` parameters stay `&str` deliberately: this trait sits below
/// the mail handler, whose wire surface accepts arbitrary caller-defined
/// topics — a `Topic`-typed backend would force a failing string-to-`Topic`
/// conversion on unknown values, re-closing the open set through the back
/// door. The typed edge for the reducer's own topics is
/// [`TopicOutbox`](crate::bloomery::TopicOutbox).
pub trait StoreBackend: Send {
    /// Record an outstanding work order at its nonce (the evidence-intake
    /// registry write side, #3502). Idempotent: a nonce already outstanding is
    /// a [`RecordOutcome::Duplicate`] no-op, never a second row.
    fn record_order(&mut self, order: &OutstandingOrder) -> rusqlite::Result<RecordOutcome>;
    /// Look an outstanding order up by nonce, or `None` if none is outstanding
    /// (never dispatched, or already consumed).
    fn lookup_order(&mut self, nonce: &str) -> rusqlite::Result<Option<OutstandingOrder>>;
    /// Consume the outstanding order at `nonce` (delete it), returning whether a
    /// row was removed. A consumed order makes a replayed nonce refuse — the
    /// consume-once semantics the trust boundary rests on.
    fn consume_order(&mut self, nonce: &str) -> rusqlite::Result<bool>;
    /// Every nonce still outstanding — the restart recovery set (issue #3641):
    /// the executor reactor's `init` seeds its in-memory tracked-handle set from
    /// this so a dispatched-but-unresolved order is polled again after a
    /// restart, rather than only from the (already-consumed-nothing) empty
    /// vec `init` used to start with.
    fn list_outstanding_nonces(&mut self) -> rusqlite::Result<Vec<String>>;

    /// Hold a parked attempt's order under the question digest that parked it
    /// (ADR-0151, #3664) — the order is consumed from `outstanding_orders` on
    /// admission, so without this the redispatch an adopted answer decides has
    /// nothing to replay. Idempotent on `(bloom, question)`: a re-admitted park
    /// overwrites rather than conflicting.
    fn record_parked_question(&mut self, question: &[u8], order: &OutstandingOrder) -> rusqlite::Result<()>;

    /// The order held under `question`, or `None` when nothing parked under it.
    /// Read before the replay dispatches and consumed only after it succeeds, so
    /// a transient dispatch failure re-drains against a row that is still there.
    fn lookup_parked_question(&mut self, bloom: &[u8], question: &[u8]) -> rusqlite::Result<Option<OutstandingOrder>>;

    /// Release the held order once its replay has dispatched. `true` when a row
    /// was removed.
    fn consume_parked_question(&mut self, bloom: &[u8], question: &[u8]) -> rusqlite::Result<bool>;
    /// Record a per-bloom study index row (issue #3523): the study artifact
    /// digest for a graded attempt, keyed by (`bloom`, `attempt_digest`).
    /// Last-writer-wins on the key — a re-admit of the same attempt overwrites,
    /// so the projection converges to the latest accepted study artifact rather
    /// than erroring, and a rebuild that re-inserts the same rows is idempotent.
    fn record_study(&mut self, bloom: &[u8], attempt_digest: &[u8], study_artifact: &str) -> rusqlite::Result<()>;
    /// The study artifact digest recorded for (`bloom`, `attempt_digest`), or
    /// `None` when no study record has been admitted for that attempt.
    fn lookup_study(&mut self, bloom: &[u8], attempt_digest: &[u8]) -> rusqlite::Result<Option<String>>;
    /// Record a member's advisory work-order description (#3595), keyed by
    /// (`bloom`, `workpiece`). The coordinator persists it at seal so it survives
    /// to dispatch — the api cap that holds the operator's text and the executor
    /// reactor that reads it at dispatch are different capabilities, so the store
    /// is the only carrier between them. Last-writer-wins on the key: a re-seal of
    /// the same member overwrites rather than erroring.
    fn record_dispatch_description(&mut self, bloom: &[u8], workpiece: &str, description: &str)
    -> rusqlite::Result<()>;
    /// The advisory work-order description recorded for (`bloom`, `workpiece`), or
    /// `None` when the coordinator persisted none — the executor reactor leaves
    /// [`Transformation::description`](aether_bloomery::Transformation) `None` and
    /// warns rather than dispatching blind.
    fn lookup_dispatch_description(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<String>>;
    /// Every persisted work-order description for one bloom as
    /// (`workpiece`, `description`) pairs in workpiece order — the aggregate
    /// review composes its task context from the whole membership's orders
    /// (ADR-0153): the sealed intent the critic judges the integrated diff
    /// against.
    fn list_dispatch_descriptions(&mut self, bloom: &[u8]) -> rusqlite::Result<Vec<(String, String)>>;
    /// Record the review critic's findings for (`bloom`, `workpiece`) (#3656) —
    /// what a Refine re-entry is directed by. Last-writer-wins on the key: a
    /// newer review's findings supersede older ones.
    fn record_review_findings(&mut self, bloom: &[u8], workpiece: &str, findings: &str) -> rusqlite::Result<()>;
    /// The review findings recorded for (`bloom`, `workpiece`), or `None` when
    /// no failing review has stamped any (or a passing review cleared them).
    fn lookup_review_findings(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<String>>;
    /// Clear the member's recorded findings — a passing review makes them stale.
    fn clear_review_findings(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<()>;
    /// Drop every study index row — the first half of a projection rebuild
    /// (`clear` then re-`record` from the artifact bytes).
    fn clear_study_index(&mut self) -> rusqlite::Result<()>;
    /// Every study index row, in (`bloom`, `attempt_digest`) order — the rebuild
    /// oracle a test folds against.
    fn study_rows(&mut self) -> rusqlite::Result<Vec<StudyRow>>;
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
    /// Read undelivered outbox entries, in sequence order — scoped to `topic`
    /// when `Some`, across every topic when `None`.
    fn drain_outbox(&mut self, topic: Option<&str>) -> rusqlite::Result<Vec<OutboxEntry>>;
    /// Mark outbox entries at or below `through_sequence` delivered — scoped to
    /// `topic` when `Some`, across every topic when `None`; returns how many
    /// were newly acknowledged.
    fn ack_outbox(&mut self, topic: Option<&str>, through_sequence: u64) -> rusqlite::Result<u32>;
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
        // A busy timeout so a second connection to the same file (the executor
        // dispatch reactor opens its own to drive the intake registry, #3505) waits
        // for the WAL write lock rather than failing fast with SQLITE_BUSY; WAL is
        // still single-writer, so the timeout serializes the rare concurrent write.
        conn.busy_timeout(Duration::from_secs(5))?;
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
CREATE TABLE IF NOT EXISTS outstanding_orders (
    nonce            TEXT PRIMARY KEY,
    bloom            BLOB NOT NULL,
    workpiece        TEXT NOT NULL,
    scope_revision   BLOB NOT NULL,
    candidate        BLOB NOT NULL,
    displayed_digest BLOB NOT NULL,
    stage            BLOB NOT NULL,
    transformation   BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS parked_question (
    bloom            BLOB NOT NULL,
    question         BLOB NOT NULL,
    nonce            TEXT NOT NULL,
    workpiece        TEXT NOT NULL,
    scope_revision   BLOB NOT NULL,
    candidate        BLOB NOT NULL,
    displayed_digest BLOB NOT NULL,
    stage            BLOB NOT NULL,
    transformation   BLOB NOT NULL,
    PRIMARY KEY (bloom, question)
);
CREATE TABLE IF NOT EXISTS study_index (
    bloom          BLOB NOT NULL,
    attempt_digest BLOB NOT NULL,
    study_artifact TEXT NOT NULL,
    PRIMARY KEY (bloom, attempt_digest)
);
CREATE TABLE IF NOT EXISTS dispatch_description (
    bloom       BLOB NOT NULL,
    workpiece   TEXT NOT NULL,
    description TEXT NOT NULL,
    PRIMARY KEY (bloom, workpiece)
);
CREATE TABLE IF NOT EXISTS review_findings (
    bloom     BLOB NOT NULL,
    workpiece TEXT NOT NULL,
    findings  TEXT NOT NULL,
    PRIMARY KEY (bloom, workpiece)
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

/// The [`OutstandingOrder`] columns, in the order [`order_from_row`] reads them.
/// Both tables that hold an order — `outstanding_orders` keyed by nonce and
/// `parked_question` keyed by the question that parked it — select through this
/// one spelling, so they cannot drift apart column-wise.
const ORDER_COLUMNS: &str =
    "nonce, bloom, workpiece, scope_revision, candidate, displayed_digest, stage, transformation";

/// Read an [`OutstandingOrder`] from a row selected with [`ORDER_COLUMNS`].
fn order_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutstandingOrder> {
    Ok(OutstandingOrder {
        nonce: row.get(0)?,
        bloom: row.get(1)?,
        workpiece: row.get(2)?,
        scope_revision: row.get(3)?,
        candidate: row.get(4)?,
        displayed_digest: row.get(5)?,
        stage: row.get(6)?,
        transformation: row.get(7)?,
    })
}

/// An [`OutstandingOrder`]'s columns as positional parameters matching
/// [`ORDER_COLUMNS`], for the two tables that insert one.
fn order_params(order: &OutstandingOrder) -> [&dyn rusqlite::ToSql; 8] {
    [
        &order.nonce,
        &order.bloom,
        &order.workpiece,
        &order.scope_revision,
        &order.candidate,
        &order.displayed_digest,
        &order.stage,
        &order.transformation,
    ]
}

impl StoreBackend for SqliteStore {
    fn record_order(&mut self, order: &OutstandingOrder) -> rusqlite::Result<RecordOutcome> {
        let changed = self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO outstanding_orders ({ORDER_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            order_params(order).as_slice(),
        )?;
        Ok(if changed == 0 {
            RecordOutcome::Duplicate
        } else {
            RecordOutcome::Recorded
        })
    }

    fn lookup_order(&mut self, nonce: &str) -> rusqlite::Result<Option<OutstandingOrder>> {
        let mut stmt =
            self.conn.prepare(&format!("SELECT {ORDER_COLUMNS} FROM outstanding_orders WHERE nonce = ?1"))?;
        let mut rows = stmt.query_map(rusqlite::params![nonce], order_from_row)?;
        // The nonce is the primary key, so there is at most one row.
        rows.next().transpose()
    }

    fn consume_order(&mut self, nonce: &str) -> rusqlite::Result<bool> {
        let removed = self.conn.execute("DELETE FROM outstanding_orders WHERE nonce = ?1", rusqlite::params![nonce])?;
        Ok(removed > 0)
    }

    fn list_outstanding_nonces(&mut self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT nonce FROM outstanding_orders")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    fn record_parked_question(&mut self, question: &[u8], order: &OutstandingOrder) -> rusqlite::Result<()> {
        // `question` leads the parameter list so the order's own columns keep the
        // ?1.. positions `order_params` produces.
        self.conn.execute(
            &format!(
                "INSERT OR REPLACE INTO parked_question (question, {ORDER_COLUMNS}) \
                 VALUES (?9, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            [order_params(order).as_slice(), &[&question as &dyn rusqlite::ToSql]].concat().as_slice(),
        )?;
        Ok(())
    }

    fn lookup_parked_question(&mut self, bloom: &[u8], question: &[u8]) -> rusqlite::Result<Option<OutstandingOrder>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {ORDER_COLUMNS} FROM parked_question WHERE bloom = ?1 AND question = ?2"))?;
        let mut rows = stmt.query_map(rusqlite::params![bloom, question], order_from_row)?;
        // `(bloom, question)` is the primary key, so there is at most one row.
        rows.next().transpose()
    }

    fn consume_parked_question(&mut self, bloom: &[u8], question: &[u8]) -> rusqlite::Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM parked_question WHERE bloom = ?1 AND question = ?2",
            rusqlite::params![bloom, question],
        )?;
        Ok(removed > 0)
    }

    fn record_study(&mut self, bloom: &[u8], attempt_digest: &[u8], study_artifact: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO study_index (bloom, attempt_digest, study_artifact) VALUES (?1, ?2, ?3)",
            rusqlite::params![bloom, attempt_digest, study_artifact],
        )?;
        Ok(())
    }

    fn lookup_study(&mut self, bloom: &[u8], attempt_digest: &[u8]) -> rusqlite::Result<Option<String>> {
        let mut stmt =
            self.conn.prepare("SELECT study_artifact FROM study_index WHERE bloom = ?1 AND attempt_digest = ?2")?;
        let mut rows = stmt.query_map(rusqlite::params![bloom, attempt_digest], |row| row.get::<_, String>(0))?;
        // The (bloom, attempt_digest) pair is the primary key, so at most one row.
        rows.next().transpose()
    }

    fn record_dispatch_description(
        &mut self,
        bloom: &[u8],
        workpiece: &str,
        description: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO dispatch_description (bloom, workpiece, description) VALUES (?1, ?2, ?3)",
            rusqlite::params![bloom, workpiece, description],
        )?;
        Ok(())
    }

    fn lookup_dispatch_description(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<String>> {
        let mut stmt =
            self.conn.prepare("SELECT description FROM dispatch_description WHERE bloom = ?1 AND workpiece = ?2")?;
        let mut rows = stmt.query_map(rusqlite::params![bloom, workpiece], |row| row.get::<_, String>(0))?;
        // The (bloom, workpiece) pair is the primary key, so at most one row.
        rows.next().transpose()
    }

    fn list_dispatch_descriptions(&mut self, bloom: &[u8]) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT workpiece, description FROM dispatch_description WHERE bloom = ?1 ORDER BY workpiece")?;
        let rows =
            stmt.query_map(rusqlite::params![bloom], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        rows.collect()
    }

    fn record_review_findings(&mut self, bloom: &[u8], workpiece: &str, findings: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO review_findings (bloom, workpiece, findings) VALUES (?1, ?2, ?3)",
            rusqlite::params![bloom, workpiece, findings],
        )?;
        Ok(())
    }

    fn lookup_review_findings(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT findings FROM review_findings WHERE bloom = ?1 AND workpiece = ?2")?;
        let mut rows = stmt.query_map(rusqlite::params![bloom, workpiece], |row| row.get::<_, String>(0))?;
        // The (bloom, workpiece) pair is the primary key, so at most one row.
        rows.next().transpose()
    }

    fn clear_review_findings(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM review_findings WHERE bloom = ?1 AND workpiece = ?2",
            rusqlite::params![bloom, workpiece],
        )?;
        Ok(())
    }

    fn clear_study_index(&mut self) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM study_index", [])?;
        Ok(())
    }

    fn study_rows(&mut self) -> rusqlite::Result<Vec<StudyRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT bloom, attempt_digest, study_artifact FROM study_index ORDER BY bloom, attempt_digest")?;
        let rows = stmt.query_map([], |row| {
            Ok(StudyRow { bloom: row.get(0)?, attempt_digest: row.get(1)?, study_artifact: row.get(2)? })
        })?;
        rows.collect()
    }

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

    fn drain_outbox(&mut self, topic: Option<&str>) -> rusqlite::Result<Vec<OutboxEntry>> {
        // The topic predicate is appended only when scoped, so `None` keeps the
        // whole-outbox drain the recovery drill uses.
        let sql = match topic {
            Some(_) => {
                "SELECT sequence, topic, payload FROM outbox WHERE delivered = 0 AND topic = ?1 ORDER BY sequence"
            }
            None => "SELECT sequence, topic, payload FROM outbox WHERE delivered = 0 ORDER BY sequence",
        };
        let mut stmt = self.conn.prepare(sql)?;
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(OutboxEntry {
                sequence: u64::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
                topic: row.get(1)?,
                payload: row.get(2)?,
            })
        };
        match topic {
            Some(topic) => stmt.query_map(rusqlite::params![topic], map_row)?.collect(),
            None => stmt.query_map([], map_row)?.collect(),
        }
    }

    fn ack_outbox(&mut self, topic: Option<&str>, through_sequence: u64) -> rusqlite::Result<u32> {
        let through = i64::try_from(through_sequence).unwrap_or(i64::MAX);
        let acked = match topic {
            Some(topic) => self.conn.execute(
                "UPDATE outbox SET delivered = 1 WHERE sequence <= ?1 AND delivered = 0 AND topic = ?2",
                rusqlite::params![through, topic],
            )?,
            None => self.conn.execute(
                "UPDATE outbox SET delivered = 1 WHERE sequence <= ?1 AND delivered = 0",
                rusqlite::params![through],
            )?,
        };
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
        tracing::info!(target: "aether_chassis_bloomery::store", path = %config.path, "store opened (WAL)");
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
    fn on_record_dispatch_description(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RecordDispatchDescription,
    ) -> RecordDispatchDescriptionResult {
        let RecordDispatchDescription { bloom, workpiece, description } = mail;
        match state.backend.record_dispatch_description(&bloom, &workpiece, &description) {
            Ok(()) => RecordDispatchDescriptionResult::Ok,
            Err(error) => RecordDispatchDescriptionResult::Err { error: error.to_string() },
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

    // The `#[handler::single]` contract requires the mail by value; these
    // handlers only read the topic / sequence, so clippy sees a by-ref
    // opportunity the macro signature cannot take.
    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_drain_outbox(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: DrainOutbox) -> DrainOutboxResult {
        match state.backend.drain_outbox(mail.topic.as_deref()) {
            Ok(entries) => DrainOutboxResult::Ok { entries },
            Err(error) => DrainOutboxResult::Err { error: error.to_string() },
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_ack_outbox(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AckOutbox) -> AckOutboxResult {
        match state.backend.ack_outbox(mail.topic.as_deref(), mail.through_sequence) {
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
