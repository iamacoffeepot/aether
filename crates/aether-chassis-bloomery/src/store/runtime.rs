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
    DrainOutboxResult, EnqueueOutbox, EnqueueOutboxResult, OutboxEntry, RecordConfig, RecordConfigResult,
    RecordDispatchDescription, RecordDispatchDescriptionResult, ReleaseMembership, ReleaseMembershipResult, Supersede,
    SupersedeResult,
};
use aether_actor::runtime;
// The control-plane transact-mails the wasm control actor drives — `Commit` and
// the `ReplayJournal` family — are defined in `aether-bloomery` to avoid a
// package cycle (the actor lives there; host depends on it). Host imports them
// inward for its `StoreCapability` handlers (issue #3497).
use aether_bloomery::{
    Commit, CommitResult, ConfigRecord, JournalRecord, LoadConfigs, LoadConfigsResult, MembershipMutation,
    OutboxPayload, ReplayJournal, ReplayJournalResult,
};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::ffi::{Error as SqliteFfiError, SQLITE_ERROR};

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
    /// The sealed [`ConfigRegistry`](aether_bloomery::ConfigRegistry) the lane
    /// runs under (ADR-0174) as its canonical `aether_data::wire` bytes, on the
    /// same reasoning as `transformation`: the reducer flattened the member's
    /// registry over the bloom's, and a replay cannot reconstruct that from the
    /// remaining columns.
    pub configs: Vec<u8>,
    /// The [`AgentProfile`](aether_bloomery::AgentProfile) the bloom's sealed
    /// stage catalog calibrates this stage at (ADR-0174) as its canonical
    /// `aether_data::wire` bytes, on the same reasoning as `configs`: the reducer
    /// resolved it from a catalog no host-side column carries, so a replay cannot
    /// reconstruct it — and re-deriving it from the compiled line would dispatch
    /// the fleet default for a bloom that sealed something else.
    pub profile: Vec<u8>,
    /// The absolute instant this order's attempt is cancelled at, in Unix
    /// milliseconds (ADR-0177).
    ///
    /// Computed once, when the host durably records the order — so queue and
    /// startup delay spend the same sealed allowance as running time — from the
    /// order's own
    /// [`ExecutionLimits::wall_clock_secs`](aether_bloomery::ExecutionLimits).
    /// Unix milliseconds because it is the only clock that survives a restart:
    /// a process-local `Instant` is renewed by the restart, which is exactly
    /// what let a hung order outlive every one of them. Never replaced on
    /// re-record or rediscovery — a deadline that moves is not a deadline.
    pub deadline_unix_millis: u64,
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

/// One journal row's content: a decided event — the idempotency key, the
/// event bytes, the wire-encoded decisions the reducer produced for it, and
/// the identity of the build that decided it (ADR-0190). Grouped because the
/// four are inseparable at every write: a row without its decision cannot be
/// replayed.
pub struct JournalWrite<'a> {
    /// The event's idempotency key — the inbox dedup axis.
    pub idempotency_key: &'a str,
    /// The event's canonical wire bytes.
    pub event: &'a [u8],
    /// The wire-encoded decisions the event reduced to at admission.
    pub decisions: &'a [u8],
    /// The identity of the build whose reducer decided the event.
    pub decider: &'a str,
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
    /// Every outstanding order whose stored deadline is at or before
    /// `now_unix_millis` — the expiry set the executor reactor terminates
    /// (ADR-0177), in nonce order so a repeated tick handles them the same way.
    ///
    /// Reads the persisted deadline rather than any process-local age, so a
    /// restart neither extends nor resets it: the same rows select again from
    /// the same numbers.
    fn list_expired_orders(&mut self, now_unix_millis: u64) -> rusqlite::Result<Vec<OutstandingOrder>>;

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
    /// Store an authored configuration's canonical bytes under its address
    /// (ADR-0174), so a sealed [`ConfigRegistry`](aether_bloomery::ConfigRegistry)
    /// entry resolves to content at the point of use. Idempotent by content
    /// addressing.
    ///
    /// `kind` rides alongside the bytes so a resolution can check that what is
    /// stored is the kind the registry key claims. The address already binds the
    /// kind — it is domain-separated by the name — so this catches a row written
    /// by some path that did not compute the address that way, rather than a
    /// mismatch the address itself would admit.
    fn record_config(&mut self, digest: &[u8], kind: &str, bytes: &[u8]) -> rusqlite::Result<()>;

    /// The configuration kind and bytes stored under `digest`, or `None` when
    /// nothing was authored for it.
    ///
    /// A `None` here is a *sealed address with no content*, which the caller
    /// must refuse rather than default past — unlike an unsealed kind, which
    /// never reaches this call at all.
    fn lookup_config(&mut self, digest: &[u8]) -> rusqlite::Result<Option<(String, Vec<u8>)>>;

    /// Every stored configuration, in address order — the whole-table read the
    /// control core fills its resolved set from (ADR-0174).
    ///
    /// Whole-table because the reducer needs content for addresses it has not
    /// seen yet: a registry names them, and the registry is what the read exists
    /// to let it resolve. The set is one row per distinct authored value.
    fn load_configs(&mut self) -> rusqlite::Result<Vec<ConfigRecord>>;

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
    /// Whether `bloom` still holds any active membership — the reducer's own
    /// answer to "is this still the live plan". A supersession releases every one
    /// of the predecessor's memberships in the same decision set that marks it
    /// superseded, so a bloom with none left is retired, and the executor reactor
    /// reads this to retire its already-queued dispatches with it (#4640).
    fn holds_active_membership(&mut self, bloom: &[u8]) -> rusqlite::Result<bool>;
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
    /// Record the commit message the member's construct/refine lane wrote for the
    /// candidate it just captured, keyed by (`bloom`, `workpiece`) exactly as the
    /// findings channel is. Last-writer-wins on the key, which is what makes the
    /// row *per candidate*: a member's only writer is the lane that captures a
    /// candidate for it, so a Refine's fresh capture supersedes the message of the
    /// candidate it replaces, and the row the land path reads at the end belongs
    /// to the candidate that resolved the member.
    fn record_candidate_commit_message(&mut self, bloom: &[u8], workpiece: &str, message: &str)
    -> rusqlite::Result<()>;
    /// The commit message recorded for (`bloom`, `workpiece`), or `None` when the
    /// member's lane wrote none — the landing assembly falls back rather than
    /// blocking on the absence.
    fn lookup_candidate_commit_message(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<String>>;
    /// Drop every study index row — the first half of a projection rebuild
    /// (`clear` then re-`record` from the artifact bytes).
    fn clear_study_index(&mut self) -> rusqlite::Result<()>;
    /// Every study index row, in (`bloom`, `attempt_digest`) order — the rebuild
    /// oracle a test folds against.
    fn study_rows(&mut self) -> rusqlite::Result<Vec<StudyRow>>;
    /// Apply a combined commit — journal the decided event, apply the
    /// membership releases then claims, and enqueue the outbox payloads — in
    /// **one** transaction (ADR-0149 §The control core). A duplicate key or a
    /// membership conflict applies nothing.
    fn commit(
        &mut self,
        write: &JournalWrite<'_>,
        releases: &[MembershipMutation],
        claims: &[MembershipMutation],
        outbox: &[OutboxPayload],
    ) -> rusqlite::Result<CommitOutcome>;
    /// Append a journal row, deduplicated by its idempotency key.
    fn append_event(&mut self, write: &JournalWrite<'_>) -> rusqlite::Result<AppendOutcome>;
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
        let mut conn = Connection::open(path)?;
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
        migrate_schema(&mut conn)?;
        Ok(Self { conn })
    }
}

/// The store's schema version, stamped in `PRAGMA user_version`.
///
/// `1` is ADR-0177's coordinated pre-1.0 break: an outstanding order gains
/// `deadline_unix_millis`, and its `transformation` column's canonical bytes
/// changed with `Transformation.limits` (#4697). Both land in this one version
/// because both invalidate exactly the same rows.
///
/// `2` is ADR-0190: a journal row records the decision its event reduced to
/// (`decisions` — wire-encoded `Decisions` — plus the `decider` build stamp),
/// so boot replay folds the record instead of re-deciding under the current
/// binary. Rows written before this version carry `NULL` decisions and refuse
/// to replay until a backfill stamps them.
const SCHEMA_VERSION: i64 = 2;

/// Bring a store opened at [`MIGRATIONS`] up to [`SCHEMA_VERSION`], or refuse it.
///
/// A store created by this build already has the current shape, so the only work
/// is stamping the version. A store from before the break has neither the
/// deadline column nor decodable `transformation` bytes, and there is no
/// truthful origin for either: fabricating a dispatch time would put a deadline
/// on the wire that no bloom ever attested, and reinterpreting the old
/// transformation bytes would silently change what a stored order means. So an
/// empty legacy store migrates mechanically and a legacy store still holding
/// order rows is refused by name, with the operator reset or export/recreate
/// cycle ADR-0177 requires.
fn migrate_schema(conn: &mut Connection) -> rusqlite::Result<()> {
    if conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? >= SCHEMA_VERSION {
        return Ok(());
    }
    // One transaction over the whole step — the counts that decide the refusal,
    // every `ALTER`, and the version stamp. Left as separate autocommits, a
    // second `ALTER` that faults (or a process that dies between two of them)
    // commits the first and skips the stamp, and the *next* open sees one
    // migrated table, concludes there is nothing to do, and stamps the version
    // over a half-migrated store: permanently "current" with `parked_question`
    // missing its column, which silently breaks the ADR-0151 park/replay path
    // for good. SQLite makes both DDL and the `user_version` header write
    // transactional, so committing them together is all-or-nothing.
    let migration = conn.transaction()?;
    // Each table is gated on its own column rather than one standing in for
    // both: they are altered independently, so only their own `PRAGMA
    // table_info` says whether they still need it — and a store already left
    // half-migrated by an earlier build repairs on this open instead of being
    // read as done.
    let mut pending = Vec::new();
    for table in ORDER_BEARING_TABLES {
        if !has_column(&migration, table, "deadline_unix_millis")? {
            pending.push(table);
        }
    }

    if !pending.is_empty() {
        let outstanding = count_rows(&migration, "outstanding_orders")?;
        let parked = count_rows(&migration, "parked_question")?;
        if outstanding > 0 || parked > 0 {
            return Err(legacy_store_refusal(outstanding, parked));
        }
        for table in pending {
            migration.execute_batch(&add_deadline_column(table))?;
        }
    }

    // ADR-0190 (version 2): the journal records its decisions. The columns are
    // added without a default — pre-existing rows read back `NULL` and are
    // refused at replay by name until a backfill stamps them, because inventing
    // a decision here would attest an outcome no reducer produced.
    if !has_column(&migration, "journal", "decisions")? {
        migration.execute_batch(
            "ALTER TABLE journal ADD COLUMN decisions BLOB;
             ALTER TABLE journal ADD COLUMN decider TEXT;",
        )?;
    }

    migration.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    migration.commit()
}

/// Whether `table` already declares `column`.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    names.try_fold(false, |found, name| Ok(found || name? == column))
}

fn count_rows(conn: &Connection, table: &str) -> rusqlite::Result<u64> {
    let counted = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(counted).unwrap_or_default())
}

/// The refusal a nonempty legacy store opens with — loud, and naming the rows
/// that make the migration untruthful rather than the version numbers.
fn legacy_store_refusal(outstanding: u64, parked: u64) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        SqliteFfiError::new(SQLITE_ERROR),
        Some(format!(
            "bloomery store predates schema version {SCHEMA_VERSION} (ADR-0177) and still holds \
             {outstanding} outstanding order(s) and {parked} parked order(s). Those rows carry no dispatch \
             deadline and no longer decodable transformation bytes, and inventing either would attest a \
             limit no bloom sealed. Reset this trial store, or export and recreate it, before reopening."
        )),
    )
}

/// The refusal a replay answers when it reaches a journal row written before
/// ADR-0190 stamped decisions onto the journal — named, with the obligation
/// stated, rather than silently re-deciding the row under the current reducer.
fn unstamped_row_refusal(sequence: u64) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        SqliteFfiError::new(SQLITE_ERROR),
        Some(format!(
            "journal row {sequence} predates ADR-0190 and records no decision. Replaying it would re-decide \
             history under the current reducer. Backfill the store (stamp each row with the decisions the \
             deciding build produced) before reopening."
        )),
    )
}

/// The tables that carry an order row, and so carry its deadline: the live one
/// and the ADR-0151 parked one. Both are migrated, and each on its own gate.
const ORDER_BEARING_TABLES: [&str; 2] = ["outstanding_orders", "parked_question"];

/// The empty-store migration for one order-bearing table: add the deadline
/// column so it matches [`MIGRATIONS`]. Only ever runs against zero rows, so the
/// default it declares is never read back as a real deadline.
fn add_deadline_column(table: &str) -> String {
    format!("ALTER TABLE {table} ADD COLUMN deadline_unix_millis INTEGER NOT NULL DEFAULT 0;")
}

/// The schema, applied idempotently on every open.
const MIGRATIONS: &str = "\
CREATE TABLE IF NOT EXISTS journal (
    sequence        INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL UNIQUE,
    event           BLOB NOT NULL,
    decisions       BLOB,
    decider         TEXT
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
    nonce                TEXT PRIMARY KEY,
    bloom                BLOB NOT NULL,
    workpiece            TEXT NOT NULL,
    scope_revision       BLOB NOT NULL,
    candidate            BLOB NOT NULL,
    displayed_digest     BLOB NOT NULL,
    stage                BLOB NOT NULL,
    transformation       BLOB NOT NULL,
    configs              BLOB NOT NULL,
    profile              BLOB NOT NULL,
    deadline_unix_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS parked_question (
    bloom                BLOB NOT NULL,
    question             BLOB NOT NULL,
    nonce                TEXT NOT NULL,
    workpiece            TEXT NOT NULL,
    scope_revision       BLOB NOT NULL,
    candidate            BLOB NOT NULL,
    displayed_digest     BLOB NOT NULL,
    stage                BLOB NOT NULL,
    transformation       BLOB NOT NULL,
    configs              BLOB NOT NULL,
    profile              BLOB NOT NULL,
    deadline_unix_millis INTEGER NOT NULL,
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
CREATE TABLE IF NOT EXISTS config (
    digest BLOB PRIMARY KEY,
    kind   TEXT NOT NULL,
    bytes  BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS review_findings (
    bloom     BLOB NOT NULL,
    workpiece TEXT NOT NULL,
    findings  TEXT NOT NULL,
    PRIMARY KEY (bloom, workpiece)
);
CREATE TABLE IF NOT EXISTS candidate_commit_message (
    bloom     BLOB NOT NULL,
    workpiece TEXT NOT NULL,
    message   TEXT NOT NULL,
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
const ORDER_COLUMNS: &str = "nonce, bloom, workpiece, scope_revision, candidate, displayed_digest, stage, \
                             transformation, configs, profile, deadline_unix_millis";

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
        configs: row.get(8)?,
        profile: row.get(9)?,
        // `SQLite` integers are signed; every deadline this store writes goes
        // through the clamp in `deadline_column`, so a negative here is a row
        // no writer of ours produced. `0` reads as immediately expired, which
        // terminates the order accountably rather than trusting a corrupt one.
        deadline_unix_millis: u64::try_from(row.get::<_, i64>(10)?).unwrap_or_default(),
    })
}

/// One deadline as the signed integer the column stores, saturating at
/// [`i64::MAX`] — a wall clock that far out is unreachable, so the clamp costs
/// nothing a real dispatch can observe.
fn deadline_column(order: &OutstandingOrder) -> i64 {
    i64::try_from(order.deadline_unix_millis).unwrap_or(i64::MAX)
}

/// An [`OutstandingOrder`]'s columns as positional parameters matching
/// [`ORDER_COLUMNS`], for the two tables that insert one. The deadline is
/// clamped by the caller into `deadline`, which the array borrows.
fn order_params<'a>(order: &'a OutstandingOrder, deadline: &'a i64) -> [&'a dyn rusqlite::ToSql; 11] {
    [
        &order.nonce,
        &order.bloom,
        &order.workpiece,
        &order.scope_revision,
        &order.candidate,
        &order.displayed_digest,
        &order.stage,
        &order.transformation,
        &order.configs,
        &order.profile,
        deadline,
    ]
}

impl StoreBackend for SqliteStore {
    fn record_order(&mut self, order: &OutstandingOrder) -> rusqlite::Result<RecordOutcome> {
        // `INSERT OR IGNORE` is also what keeps a deadline immutable: a
        // re-recorded nonce changes no column, so a redrive cannot extend the
        // allowance of an order already in flight.
        let deadline = deadline_column(order);
        let changed = self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO outstanding_orders ({ORDER_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            order_params(order, &deadline).as_slice(),
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

    fn list_expired_orders(&mut self, now_unix_millis: u64) -> rusqlite::Result<Vec<OutstandingOrder>> {
        let now = i64::try_from(now_unix_millis).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ORDER_COLUMNS} FROM outstanding_orders WHERE deadline_unix_millis <= ?1 ORDER BY nonce"
        ))?;
        let rows = stmt.query_map(rusqlite::params![now], order_from_row)?;
        rows.collect()
    }

    fn record_parked_question(&mut self, question: &[u8], order: &OutstandingOrder) -> rusqlite::Result<()> {
        // `question` leads the parameter list so the order's own columns keep the
        // ?1.. positions `order_params` produces.
        let deadline = deadline_column(order);
        self.conn.execute(
            &format!(
                "INSERT OR REPLACE INTO parked_question (question, {ORDER_COLUMNS}) \
                 VALUES (?12, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
            ),
            [order_params(order, &deadline).as_slice(), &[&question as &dyn rusqlite::ToSql]].concat().as_slice(),
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

    fn record_config(&mut self, digest: &[u8], kind: &str, bytes: &[u8]) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO config (digest, kind, bytes) VALUES (?1, ?2, ?3)",
            rusqlite::params![digest, kind, bytes],
        )?;
        Ok(())
    }

    fn load_configs(&mut self) -> rusqlite::Result<Vec<ConfigRecord>> {
        let mut stmt = self.conn.prepare("SELECT digest, kind, bytes FROM config ORDER BY digest")?;
        let rows =
            stmt.query_map([], |row| Ok(ConfigRecord { digest: row.get(0)?, kind: row.get(1)?, bytes: row.get(2)? }))?;

        rows.collect()
    }

    fn lookup_config(&mut self, digest: &[u8]) -> rusqlite::Result<Option<(String, Vec<u8>)>> {
        let mut stmt = self.conn.prepare("SELECT kind, bytes FROM config WHERE digest = ?1")?;
        let mut rows =
            stmt.query_map(rusqlite::params![digest], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?;
        // The digest is the primary key, so there is at most one row.
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

    fn holds_active_membership(&mut self, bloom: &[u8]) -> rusqlite::Result<bool> {
        let mut stmt = self.conn.prepare("SELECT 1 FROM active_membership WHERE bloom = ?1 LIMIT 1")?;
        Ok(stmt.query_map(rusqlite::params![bloom], |_| Ok(()))?.next().transpose()?.is_some())
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

    fn record_candidate_commit_message(
        &mut self,
        bloom: &[u8],
        workpiece: &str,
        message: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO candidate_commit_message (bloom, workpiece, message) VALUES (?1, ?2, ?3)",
            rusqlite::params![bloom, workpiece, message],
        )?;
        Ok(())
    }

    fn lookup_candidate_commit_message(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<String>> {
        let mut stmt =
            self.conn.prepare("SELECT message FROM candidate_commit_message WHERE bloom = ?1 AND workpiece = ?2")?;
        let mut rows = stmt.query_map(rusqlite::params![bloom, workpiece], |row| row.get::<_, String>(0))?;
        // The (bloom, workpiece) pair is the primary key, so at most one row.
        rows.next().transpose()
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
        write: &JournalWrite<'_>,
        releases: &[MembershipMutation],
        claims: &[MembershipMutation],
        outbox: &[OutboxPayload],
    ) -> rusqlite::Result<CommitOutcome> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "INSERT OR IGNORE INTO journal (idempotency_key, event, decisions, decider) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![write.idempotency_key, write.event, write.decisions, write.decider],
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

    fn append_event(&mut self, write: &JournalWrite<'_>) -> rusqlite::Result<AppendOutcome> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO journal (idempotency_key, event, decisions, decider) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![write.idempotency_key, write.event, write.decisions, write.decider],
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
        let mut stmt = self
            .conn
            .prepare("SELECT sequence, idempotency_key, event, decisions, decider FROM journal ORDER BY sequence")?;
        let rows = stmt.query_map([], |row| {
            let sequence = u64::try_from(row.get::<_, i64>(0)?).unwrap_or_default();
            // A pre-ADR-0190 row carries no recorded decision, and re-deciding it
            // here is exactly the history rewrite the record exists to prevent —
            // refuse the replay by name until a backfill stamps it.
            let decisions = row.get::<_, Option<Vec<u8>>>(3)?.ok_or_else(|| unstamped_row_refusal(sequence))?;
            Ok(JournalRecord {
                sequence,
                idempotency_key: row.get(1)?,
                event: row.get(2)?,
                decisions,
                decider: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
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
        let Commit { idempotency_key, event, decisions, decider, releases, claims, outbox } = mail;
        let write =
            JournalWrite { idempotency_key: &idempotency_key, event: &event, decisions: &decisions, decider: &decider };
        match state.backend.commit(&write, &releases, &claims, &outbox) {
            Ok(CommitOutcome::Applied(sequence)) => CommitResult::Applied { idempotency_key, sequence },
            Ok(CommitOutcome::Duplicate) => CommitResult::Duplicate { idempotency_key },
            Ok(CommitOutcome::Conflict(workpiece)) => CommitResult::Conflict { idempotency_key, workpiece },
            Err(error) => CommitResult::Err { idempotency_key, error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_append_event(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AppendEvent) -> AppendEventResult {
        let AppendEvent { idempotency_key, event, decisions, decider } = mail;
        let write =
            JournalWrite { idempotency_key: &idempotency_key, event: &event, decisions: &decisions, decider: &decider };
        match state.backend.append_event(&write) {
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
    fn on_record_config(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: RecordConfig) -> RecordConfigResult {
        let RecordConfig { digest, kind, bytes } = mail;
        match state.backend.record_config(&digest, &kind, &bytes) {
            Ok(()) => RecordConfigResult::Ok { digest, kind, bytes },
            Err(error) => RecordConfigResult::Err { error: error.to_string() },
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
    fn on_load_configs(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: LoadConfigs) -> LoadConfigsResult {
        match state.backend.load_configs() {
            Ok(records) => LoadConfigsResult::Ok { records },
            Err(error) => LoadConfigsResult::Err { error: error.to_string() },
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
