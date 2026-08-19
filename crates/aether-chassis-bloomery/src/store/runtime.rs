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
use super::commission::{
    CancelCommission, CancelCommissionResult, CommissionBackend, CommissionError, CreateCommission,
    CreateCommissionResult, ListCommissions, ListCommissionsResult, ListedCommission, LoadCommission,
    LoadCommissionResult, RecordCommissionApproval, RecordCommissionApprovalResult, RecordCommissionProjection,
    RecordCommissionProjectionResult, WriteScopeRevision, WriteScopeRevisionResult,
};
use super::kinds::{
    AckOutbox, AckOutboxResult, AppendEvent, AppendEventResult, BloomDispatchLive, BloomDispatchRollup, ClaimSeal,
    ClaimSealResult, DrainOutbox, DrainOutboxResult, EnqueueOutbox, EnqueueOutboxResult, ListBloomDispatches,
    ListBloomDispatchesResult, LookupDispatch, LookupDispatchResult, OutboxEntry, PageJournal, PageJournalResult,
    RecordConfig, RecordConfigResult, RecordDispatchDescription, RecordDispatchDescriptionResult, ReleaseMembership,
    ReleaseMembershipResult, Supersede, SupersedeResult,
};
use aether_actor::runtime;
// The control-plane transact-mails the wasm control actor drives — `Commit` and
// the `ReplayJournal` family — are defined in `aether-bloomery` to avoid a
// package cycle (the actor lives there; host depends on it). Host imports them
// inward for its `StoreCapability` handlers (issue #3497).
use aether_bloomery::{
    Commit, CommitResult, ConfigRecord, DECISIONS_SCHEMA, Decision, Event, JournalRecord, LoadConfigs,
    LoadConfigsResult, MembershipMutation, MetricDispatch, MetricsLedger, OutboxPayload, ReplayJournal,
    ReplayJournalResult, ScopeRevision, Statement, Topic, WorkpieceId, decode_recorded_decisions,
};
use aether_data::wire::{from_bytes, to_vec};
use std::iter::repeat_n;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// One append-only proof-fact row (ADR-0200). Column order is the wire:
/// a reshape is a migration, not an incidental edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofFactRow {
    /// Monotonic append sequence.
    pub sequence: u64,
    /// The 32-byte closure key this fact is addressed by.
    pub closure_key: Vec<u8>,
    /// The test the result is about.
    pub test_id: String,
    /// `"green"` or `"red"` — the only two spellings this table writes.
    pub result: String,
    /// The coordinator-supplied host class the fact was proved on.
    pub host_class: String,
    /// The dispatch nonce that produced the fact, for audit.
    pub producing_dispatch: String,
    /// The bloom that produced the fact (its `BloomId` digest bytes).
    pub producing_bloom: Vec<u8>,
}

/// Columns of one proof-fact insert. Sequence is assigned by the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofFactWrite<'a> {
    /// The 32-byte closure key this fact is addressed by.
    pub closure_key: &'a [u8],
    /// The test the result is about.
    pub test_id: &'a str,
    /// `"green"` or `"red"`.
    pub result: &'a str,
    /// The coordinator-supplied host class the fact was proved on.
    pub host_class: &'a str,
    /// The dispatch nonce that produced the fact.
    pub producing_dispatch: &'a str,
    /// The bloom that produced the fact (its `BloomId` digest bytes).
    pub producing_bloom: &'a [u8],
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
    /// The bloom that dispatched `nonce`, or `None` when this store has never
    /// recorded that dispatch. Survives [`consume_order`](Self::consume_order):
    /// the janitor still has to know which bloom a consumed evidence directory
    /// belongs to so it can honour that bloom's retention window.
    fn lookup_dispatch_owner(&mut self, nonce: &str) -> rusqlite::Result<Option<Vec<u8>>>;

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
    /// Every persisted *member* work-order description for one bloom as
    /// (`workpiece`, `description`) pairs in workpiece order — the aggregate
    /// review composes its task context from the whole membership's orders
    /// (ADR-0153): the sealed intent the critic judges the integrated diff
    /// against. The reserved composition workpiece's generated refine order is
    /// keyed the same way but is not a sealed member, so it is omitted here and
    /// read only via [`Self::lookup_dispatch_description`].
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
    /// Record the construct lane's harness session id for (`bloom`, `workpiece`)
    /// (#5177) — what a same-member Refine dispatch resumes. Last-writer-wins
    /// on the key: a later construct capture supersedes the handle of the
    /// session it replaces.
    fn record_construct_session(
        &mut self,
        bloom: &[u8],
        workpiece: &str,
        session_id: &str,
        context_tokens: u64,
    ) -> rusqlite::Result<()>;
    /// The construct session recorded for (`bloom`, `workpiece`), or `None`
    /// when construct never journaled a handle — Refine then launches fresh.
    fn lookup_construct_session(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<(String, u64)>>;
    /// The construct session plus its deposit time, for a predecessor-resume
    /// warmth gate (#5178). `deposited_unix` is `None` on a pre-column row.
    fn lookup_construct_session_meta(
        &mut self,
        bloom: &[u8],
        workpiece: &str,
    ) -> rusqlite::Result<Option<(String, u64, Option<u64>)>>;
    /// Record the construct session at an explicit unix-seconds deposit time —
    /// the clock the warmth gate compares.
    fn record_construct_session_at(
        &mut self,
        bloom: &[u8],
        workpiece: &str,
        session_id: &str,
        context_tokens: u64,
        deposited_unix: u64,
    ) -> rusqlite::Result<()>;
    /// Replace the sealed member-dependency graph for `bloom` (#5178). Each
    /// pair is `(member, depends_on)`.
    fn record_member_dependencies(&mut self, bloom: &[u8], edges: &[(String, String)]) -> rusqlite::Result<()>;
    /// Direct predecessors of `workpiece` on `bloom`, in workpiece order.
    fn lookup_predecessors(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Vec<String>>;
    /// Record the diff a lane's captured candidate carries, keyed by the nonce
    /// of the order that produced it (#4959) — what the repair-lap triage reads
    /// before a passing `Refine` result is admitted. Written by an executor
    /// backend that commits the capture itself and so holds the diff; a backend
    /// that captures nothing writes nothing, and the triage passes untriaged.
    fn record_capture_diff(&mut self, nonce: &str, diff: &str) -> rusqlite::Result<()>;
    /// The capture diff recorded for `nonce`, or `None` when the lap's backend
    /// filed none.
    fn lookup_capture_diff(&mut self, nonce: &str) -> rusqlite::Result<Option<String>>;
    /// Drop the capture diff for `nonce` — the order it belongs to has been
    /// consumed, so nothing will read it again.
    fn clear_capture_diff(&mut self, nonce: &str) -> rusqlite::Result<()>;
    /// Record the fold-conflict overlay the reconcile work order assembles
    /// (ADR-0189): the contract, the conflicting paths, and the conflicted
    /// candidate tree. Last-writer-wins on the key.
    fn record_fold_conflict(&mut self, bloom: &[u8], workpiece: &str, overlay: &str) -> rusqlite::Result<()>;
    /// The fold-conflict overlay recorded for (`bloom`, `workpiece`), or
    /// `None` when no collision has stamped one.
    fn lookup_fold_conflict(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<String>>;
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
    /// Read `topic`'s *acknowledged* entries, in sequence order — the drain's
    /// mirror image (#4956).
    ///
    /// An ack says the reactor took responsibility for the entry, not that
    /// anything came of it: the payload stays on the row, so a boot that finds
    /// the responsibility unmet can re-derive the dispatch from the same bytes
    /// the first drain read. Nothing else needs delivered rows, which is why the
    /// ordinary drain is undelivered-only.
    fn delivered_outbox(&mut self, topic: &str) -> rusqlite::Result<Vec<OutboxEntry>>;
    /// Return one acknowledged entry to the undelivered queue, so the ordinary
    /// drain picks it up again; `true` when a row moved.
    ///
    /// Scoped to a single sequence rather than a prefix because an unearned ack
    /// is a statement about one entry — its neighbours were acked on their own
    /// merits, and a prefix reset would re-run them too.
    fn redeliver_outbox(&mut self, topic: &str, sequence: u64) -> rusqlite::Result<bool>;
    /// Whether the journal holds a row under any of `keys` — the "has this been
    /// accounted for" read (#4956).
    ///
    /// Takes the whole candidate set in one query because the caller's question
    /// is about the set, not any one key: a dispatch admits under exactly one of
    /// several shapes, and asking key-by-key would let a caller stop early and
    /// call a completed dispatch unaccounted for.
    fn journal_holds_any(&mut self, keys: &[String]) -> rusqlite::Result<bool>;
    /// Read the whole journal, in sequence order — the recovery replay source.
    fn replay_journal(&mut self) -> rusqlite::Result<Vec<JournalRecord>>;
    /// The host-clock stamp written on each journal row at admission, in
    /// sequence order. `None` is a row written before the column existed:
    /// reconstruct from other sources and say so. Never invented.
    fn journal_recorded_unix_millis(&mut self) -> rusqlite::Result<Vec<Option<u64>>>;
    /// Read every journaled event's bytes, in sequence order — the facts alone,
    /// without the recorded decisions replay folds (#4957).
    ///
    /// Separate from [`replay_journal`](Self::replay_journal) rather than a
    /// projection of it, because the two answer different questions and fail
    /// differently. Replay must refuse a row that records no decision (ADR-0190:
    /// re-deciding it would rewrite history), which is the correct posture for
    /// rebuilding a snapshot and the wrong one for a reader that only wants to
    /// know what an operator said — a pre-ADR-0190 row would turn a landing into
    /// a hard failure over a proposal-body sentence.
    fn list_events(&mut self) -> rusqlite::Result<Vec<Vec<u8>>>;
    /// Append discriminated proof facts (ADR-0200). Insert-only: a later
    /// write never updates or deletes an earlier row, and a stale closure
    /// key is left in place. The caller is the verify path after flake
    /// discrimination — this method does not judge whether a result earned
    /// a row.
    fn append_proof_facts(&mut self, facts: &[ProofFactWrite<'_>]) -> rusqlite::Result<()>;
    /// Every proof-fact row, in append order — the test oracle and the
    /// consultation read a later slice will key.
    fn list_proof_facts(&mut self) -> rusqlite::Result<Vec<ProofFactRow>>;
    /// Drop the metrics rollup cache. The next
    /// [`fold_metrics_from_journal`](Self::fold_metrics_from_journal) rebuilds
    /// it from the journal — the tables are cache, never truth.
    fn clear_metrics(&mut self) -> rusqlite::Result<()>;
    /// Highest journal sequence the cache has consumed, or `0` when empty.
    fn metrics_cursor(&mut self) -> rusqlite::Result<u64>;
    /// Fold journal rows after the cursor into the cache. A zero cursor is a
    /// full rebuild. Returns the resulting ledger.
    fn fold_metrics_from_journal(&mut self) -> rusqlite::Result<MetricsLedger>;
    /// Persist a ledger's rows and cursor. Idempotent replace on the fold
    /// identity.
    fn persist_metrics(&mut self, ledger: &MetricsLedger) -> rusqlite::Result<()>;
    /// Wire-encoded dispatch payloads in sequence order — the determinism oracle.
    fn metric_dispatch_payloads(&mut self) -> rusqlite::Result<Vec<Vec<u8>>>;
    /// Fold evidence-only scalars onto the dispatch row for `nonce`.
    fn record_metric_evidence(
        &mut self,
        nonce: &str,
        session_reuse_arm: Option<&str>,
        session_reuse_saved_micro_usd: Option<u64>,
        peak_resident_bytes: Option<u64>,
        calls_json: Option<&str>,
    ) -> rusqlite::Result<()>;
    /// Rollup dispatch rows for one bloom, oldest first.
    fn list_bloom_dispatch_rollup(&mut self, bloom: &[u8]) -> rusqlite::Result<Vec<BloomDispatchRollup>>;
    /// Outstanding orders for one bloom, in nonce order.
    fn list_bloom_dispatch_live(&mut self, bloom: &[u8]) -> rusqlite::Result<Vec<BloomDispatchLive>>;
    /// The bloom that names `nonce` in `dispatch_owners`, `outstanding_orders`, or
    /// `metric_dispatch` — `None` when the journal has never heard of it.
    fn lookup_named_dispatch(&mut self, nonce: &str) -> rusqlite::Result<Option<Vec<u8>>>;
    /// Rebuild the metrics cache when the journal has moved past the cursor.
    fn ensure_metrics(&mut self) -> rusqlite::Result<()>;
}

/// A WAL-mode `SQLite` store. Opening runs the migrations idempotently, so
/// reopening the same file on restart resumes against the persisted journal.
pub struct SqliteStore {
    pub(super) conn: Connection,
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
        // Foreign keys are per-connection and default OFF. Existing tables have
        // no REFERENCES, so turning the pragma on does not change their DML.
        // The commission tables do use REFERENCES; enforcement is a deliberate
        // migration decision (ADR-0199), not a free property of sharing this
        // connection.
        conn.pragma_update(None, "foreign_keys", "ON")?;
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
///
/// `3` is ADR-0187's journal half: a decided row names the writing schema of
/// its `decisions` blob. Pre-existing decided rows are stamped with the
/// identity current at migration — they already decode under it, and leaving
/// them unstamped would treat every later reshape as implicit-current.
///
/// `4` is ADR-0200: the `proof_facts` ledger. Append-only rows, created empty.
/// Existing stores gain the table; nothing is backfilled — a fact that was
/// never discriminated is not invented. Stale closure keys are not deleted.
///
/// `5` is the journal envelope's host-clock stamp (`recorded_unix_millis`).
/// New admissions write the clock the intake already uses for deadlines.
/// Pre-existing rows stay `NULL` — inventing a time on a historical row is
/// worse than an absent one, and every consumer treats `NULL` as reconstruct
/// from other sources.
///
/// `6` is the metrics rollup cache (`metric_dispatch` / `metric_bloom` /
/// `metric_day` / `metric_cursor`). Tables are cache, never truth: delete and
/// refold repairs them. Empty on creation; nothing is backfilled here — the
/// first open after migrate folds from the journal.
///
/// `7` is the commission store (ADR-0199): `commissions`,
/// `commission_statements`, `scope_revisions`, `commission_approvals`. Empty
/// on creation; nothing is backfilled — a GitHub issue body is not a signed
/// commission. Foreign-key enforcement is switched on per connection after
/// migrate, not by this version stamp.
///
/// `8` is the commission replica-issue number (`commission_projections`).
/// Empty on creation; nothing is backfilled — a GitHub issue is not adopted.
///
/// `9` is the ADR store (ADR-0201): `adrs`, `adr_transitions`. Empty on
/// creation; nothing is backfilled — a markdown file is not a signed ADR.
///
/// `10` is the per-member construct session handle (#5177):
/// `construct_session`. Empty on creation; nothing is backfilled — a
/// session id that was never captured is not invented.
///
/// `11` is the predecessor-resume journal (#5178): `construct_session`
/// gains `deposited_unix` (warmth), and `member_dependency` holds the sealed
/// graph a dependent construct looks up at unblock. Empty on creation; a
/// pre-column row stays `NULL` (not assumed warm) and nothing is backfilled.
const SCHEMA_VERSION: i64 = 11;

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

    // ADR-0187 (version 3): decided rows name the schema that wrote them.
    // Existing decided rows are stamped with the identity current at this
    // migration — they already decode under it. Unstamped (NULL decisions)
    // rows stay NULL and still refuse as pre-ADR-0190.
    if !has_column(&migration, "journal", "decisions_schema")? {
        migration.execute_batch("ALTER TABLE journal ADD COLUMN decisions_schema TEXT;")?;
        migration.execute(
            "UPDATE journal SET decisions_schema = ?1 WHERE decisions_schema IS NULL AND decisions IS NOT NULL",
            rusqlite::params![DECISIONS_SCHEMA],
        )?;
    }

    // ADR-0200 (version 4): the proof-fact ledger. Empty on creation; a
    // pre-existing store has no facts to invent, and a stale key is left in
    // place — lookup simply misses once the tree moves.
    if !has_table(&migration, "proof_facts")? {
        migration.execute_batch(PROOF_FACTS_TABLE)?;
    }

    // Version 5: the journal envelope's host-clock stamp. Added nullable
    // with no default and no backfill — a pre-existing row stays NULL.
    if !has_column(&migration, "journal", "recorded_unix_millis")? {
        migration.execute_batch("ALTER TABLE journal ADD COLUMN recorded_unix_millis INTEGER;")?;
    }

    // Version 6: metrics rollup cache. Empty on creation; a pre-existing
    // store has no invented rollups — the first open folds them from the
    // journal.
    if !has_table(&migration, "metric_cursor")? {
        migration.execute_batch(METRICS_TABLES)?;
    }

    // Version 7 (ADR-0199): the commission store. Empty on creation; a
    // pre-existing store has no signed commissions to invent from issue
    // bodies.
    if !has_table(&migration, "commissions")? {
        migration.execute_batch(super::commission::COMMISSION_TABLES)?;
    }

    // Version 8 (ADR-0199): the persisted replica-issue number. Empty on
    // creation; a pre-existing store has no owned issues to invent.
    if !has_table(&migration, "commission_projections")? {
        migration.execute_batch(super::commission::COMMISSION_PROJECTION_TABLE)?;
    }

    // Version 9 (ADR-0201): architecture decision records. Empty on
    // creation; a pre-existing store has no signed ADRs to invent from
    // markdown files.
    if !has_table(&migration, "adrs")? {
        migration.execute_batch(super::adr::ADR_TABLES)?;
    }

    // Version 10 (#5177): the construct session a same-member refine resumes.
    // Empty on creation; a pre-existing store has no captured handles to invent.
    if !has_table(&migration, "construct_session")? {
        migration.execute_batch(CONSTRUCT_SESSION_TABLE)?;
    }

    // Version 11 (#5178): deposit time on the construct session, and the sealed
    // member graph a dependent looks up at unblock. No backfill — a missing
    // deposit is stale, and a missing graph launches the dependent fresh.
    if has_table(&migration, "construct_session")? && !has_column(&migration, "construct_session", "deposited_unix")? {
        migration.execute_batch("ALTER TABLE construct_session ADD COLUMN deposited_unix INTEGER;")?;
    }
    if !has_table(&migration, "member_dependency")? {
        migration.execute_batch(MEMBER_DEPENDENCY_TABLE)?;
    }

    migration.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    migration.commit()
}

/// Whether `table` already exists.
fn has_table(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let found = conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
        rusqlite::params![table],
        |_| Ok(()),
    );
    match found {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(error) => Err(error),
    }
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
    decider         TEXT,
    decisions_schema TEXT,
    recorded_unix_millis INTEGER
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
CREATE TABLE IF NOT EXISTS dispatch_owners (
    nonce TEXT PRIMARY KEY,
    bloom BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS capture_diff (
    nonce TEXT PRIMARY KEY,
    diff  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS fold_conflict (
    bloom     BLOB NOT NULL,
    workpiece TEXT NOT NULL,
    overlay   TEXT NOT NULL,
    PRIMARY KEY (bloom, workpiece)
);
CREATE TABLE IF NOT EXISTS proof_facts (
    sequence            INTEGER PRIMARY KEY AUTOINCREMENT,
    closure_key         BLOB NOT NULL,
    test_id             TEXT NOT NULL,
    result              TEXT NOT NULL,
    host_class          TEXT NOT NULL,
    producing_dispatch  TEXT NOT NULL,
    producing_bloom     BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS metric_dispatch (
    id TEXT PRIMARY KEY,
    nonce TEXT,
    sequence INTEGER NOT NULL,
    bloom BLOB NOT NULL,
    payload BLOB NOT NULL,
    session_reuse_arm TEXT,
    session_reuse_saved_micro_usd INTEGER,
    peak_resident_bytes INTEGER,
    calls_json TEXT
);
CREATE TABLE IF NOT EXISTS metric_bloom (
    bloom BLOB PRIMARY KEY,
    seal_sequence INTEGER NOT NULL,
    payload BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS metric_day (
    label TEXT PRIMARY KEY,
    payload BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS metric_cursor (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    through_sequence INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS construct_session (
    bloom           BLOB NOT NULL,
    workpiece       TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    context_tokens  INTEGER NOT NULL,
    deposited_unix  INTEGER,
    PRIMARY KEY (bloom, workpiece)
);
CREATE TABLE IF NOT EXISTS member_dependency (
    bloom      BLOB NOT NULL,
    member     TEXT NOT NULL,
    depends_on TEXT NOT NULL,
    PRIMARY KEY (bloom, member, depends_on)
);
";

/// The ADR-0200 proof-fact ledger. Column order is load-bearing from row one.
const PROOF_FACTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS proof_facts (
    sequence            INTEGER PRIMARY KEY AUTOINCREMENT,
    closure_key         BLOB NOT NULL,
    test_id             TEXT NOT NULL,
    result              TEXT NOT NULL,
    host_class          TEXT NOT NULL,
    producing_dispatch  TEXT NOT NULL,
    producing_bloom     BLOB NOT NULL
);
";

/// The per-member construct session a same-member refine resumes (#5177).
const CONSTRUCT_SESSION_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS construct_session (
    bloom           BLOB NOT NULL,
    workpiece       TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    context_tokens  INTEGER NOT NULL,
    deposited_unix  INTEGER,
    PRIMARY KEY (bloom, workpiece)
);
";

/// The sealed member-dependency graph a dependent construct looks up (#5178).
const MEMBER_DEPENDENCY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS member_dependency (
    bloom      BLOB NOT NULL,
    member     TEXT NOT NULL,
    depends_on TEXT NOT NULL,
    PRIMARY KEY (bloom, member, depends_on)
);
";

const METRICS_TABLES: &str = "\
CREATE TABLE IF NOT EXISTS metric_dispatch (
    id TEXT PRIMARY KEY,
    nonce TEXT,
    sequence INTEGER NOT NULL,
    bloom BLOB NOT NULL,
    payload BLOB NOT NULL,
    session_reuse_arm TEXT,
    session_reuse_saved_micro_usd INTEGER,
    peak_resident_bytes INTEGER,
    calls_json TEXT
);
CREATE TABLE IF NOT EXISTS metric_bloom (
    bloom BLOB PRIMARY KEY,
    seal_sequence INTEGER NOT NULL,
    payload BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS metric_day (
    label TEXT PRIMARY KEY,
    payload BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS metric_cursor (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    through_sequence INTEGER NOT NULL
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

/// The current wall clock in Unix milliseconds — the same host-clock reading
/// intake uses to seal a dispatch deadline (ADR-0177). A clock before the
/// epoch is not a time any stamp can use, so it reads as `0`; that is a
/// recorded reading, distinct from a pre-column `NULL`.
#[must_use]
pub fn now_unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn unix_now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// One admission stamp as the signed integer the column stores, saturating at
/// [`i64::MAX`] — the same clamp [`deadline_column`] applies.
fn recorded_column(now_unix_millis: u64) -> i64 {
    i64::try_from(now_unix_millis).unwrap_or(i64::MAX)
}

/// A nullable envelope stamp as the host-clock reading it stores. A negative
/// is not a time this writer produces, so it reads as absent rather than as
/// an invented instant.
fn recorded_from_column(value: Option<i64>) -> Option<u64> {
    value.and_then(|millis| u64::try_from(millis).ok())
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
        // The owner row outlives the outstanding row: consume deletes the
        // latter so intake can refuse a replayed nonce, but the janitor still
        // has to name the bloom a leftover evidence directory belongs to.
        self.conn.execute(
            "INSERT OR IGNORE INTO dispatch_owners (nonce, bloom) VALUES (?1, ?2)",
            rusqlite::params![&order.nonce, &order.bloom],
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

    fn lookup_dispatch_owner(&mut self, nonce: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        let mut stmt = self.conn.prepare("SELECT bloom FROM dispatch_owners WHERE nonce = ?1")?;
        let mut rows = stmt.query_map(rusqlite::params![nonce], |row| row.get(0))?;
        rows.next().transpose()
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
        let mut listed = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        listed.retain(|(workpiece, _)| workpiece != WorkpieceId::COMPOSITION);
        Ok(listed)
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

    fn record_construct_session(
        &mut self,
        bloom: &[u8],
        workpiece: &str,
        session_id: &str,
        context_tokens: u64,
    ) -> rusqlite::Result<()> {
        self.record_construct_session_at(bloom, workpiece, session_id, context_tokens, unix_now_secs())
    }

    fn record_construct_session_at(
        &mut self,
        bloom: &[u8],
        workpiece: &str,
        session_id: &str,
        context_tokens: u64,
        deposited_unix: u64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO construct_session \
             (bloom, workpiece, session_id, context_tokens, deposited_unix) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                bloom,
                workpiece,
                session_id,
                i64::try_from(context_tokens).unwrap_or(i64::MAX),
                i64::try_from(deposited_unix).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    fn lookup_construct_session(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<(String, u64)>> {
        Ok(self.lookup_construct_session_meta(bloom, workpiece)?.map(|(id, context, _)| (id, context)))
    }

    fn lookup_construct_session_meta(
        &mut self,
        bloom: &[u8],
        workpiece: &str,
    ) -> rusqlite::Result<Option<(String, u64, Option<u64>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, context_tokens, deposited_unix FROM construct_session \
             WHERE bloom = ?1 AND workpiece = ?2",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![bloom, workpiece], |row| {
            let session_id = row.get::<_, String>(0)?;
            let context = row.get::<_, i64>(1)?;
            let deposited = row.get::<_, Option<i64>>(2)?.and_then(|unix| u64::try_from(unix).ok());
            Ok((session_id, u64::try_from(context).unwrap_or(0), deposited))
        })?;
        rows.next().transpose()
    }

    fn record_member_dependencies(&mut self, bloom: &[u8], edges: &[(String, String)]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM member_dependency WHERE bloom = ?1", rusqlite::params![bloom])?;
        for (member, depends_on) in edges {
            tx.execute(
                "INSERT INTO member_dependency (bloom, member, depends_on) VALUES (?1, ?2, ?3)",
                rusqlite::params![bloom, member, depends_on],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn lookup_predecessors(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT depends_on FROM member_dependency WHERE bloom = ?1 AND member = ?2 ORDER BY depends_on")?;
        let rows = stmt.query_map(rusqlite::params![bloom, workpiece], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    fn record_capture_diff(&mut self, nonce: &str, diff: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO capture_diff (nonce, diff) VALUES (?1, ?2)",
            rusqlite::params![nonce, diff],
        )?;
        Ok(())
    }

    fn lookup_capture_diff(&mut self, nonce: &str) -> rusqlite::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT diff FROM capture_diff WHERE nonce = ?1")?;
        let mut rows = stmt.query_map(rusqlite::params![nonce], |row| row.get::<_, String>(0))?;
        // The nonce is the primary key, so at most one row.
        rows.next().transpose()
    }

    fn clear_capture_diff(&mut self, nonce: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM capture_diff WHERE nonce = ?1", rusqlite::params![nonce])?;
        Ok(())
    }

    fn record_fold_conflict(&mut self, bloom: &[u8], workpiece: &str, overlay: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO fold_conflict (bloom, workpiece, overlay) VALUES (?1, ?2, ?3)",
            rusqlite::params![bloom, workpiece, overlay],
        )?;
        Ok(())
    }

    fn lookup_fold_conflict(&mut self, bloom: &[u8], workpiece: &str) -> rusqlite::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT overlay FROM fold_conflict WHERE bloom = ?1 AND workpiece = ?2")?;
        let mut rows = stmt.query_map(rusqlite::params![bloom, workpiece], |row| row.get::<_, String>(0))?;
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
        let recorded = recorded_column(now_unix_millis());
        let changed = tx.execute(
            "INSERT OR IGNORE INTO journal \
             (idempotency_key, event, decisions, decider, decisions_schema, recorded_unix_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                write.idempotency_key,
                write.event,
                write.decisions,
                write.decider,
                DECISIONS_SCHEMA,
                recorded,
            ],
        )?;
        if changed == 0 {
            // The key was already journaled — the whole commit is a no-op. The
            // transaction rolls back on drop, so no membership/outbox row applies
            // twice on a replayed key (the durable inbox-dedup backstop).
            return Ok(CommitOutcome::Duplicate);
        }
        // A rowid is a non-negative i64; the fallback never triggers.
        let sequence = u64::try_from(tx.last_insert_rowid()).unwrap_or_default();
        let recorded_unix_millis = recorded_from_column(Some(recorded));
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
        let _ = self.upsert_metrics_from_write(sequence, write, recorded_unix_millis);
        self.persist_member_graph(write.decisions);
        Ok(CommitOutcome::Applied(sequence))
    }

    fn append_event(&mut self, write: &JournalWrite<'_>) -> rusqlite::Result<AppendOutcome> {
        let recorded = recorded_column(now_unix_millis());
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO journal \
             (idempotency_key, event, decisions, decider, decisions_schema, recorded_unix_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                write.idempotency_key,
                write.event,
                write.decisions,
                write.decider,
                DECISIONS_SCHEMA,
                recorded,
            ],
        )?;
        if changed == 0 {
            Ok(AppendOutcome::Duplicate)
        } else {
            // A rowid is a non-negative i64; the fallback never triggers.
            let sequence = u64::try_from(self.conn.last_insert_rowid()).unwrap_or_default();
            let _ = self.upsert_metrics_from_write(sequence, write, recorded_from_column(Some(recorded)));
            self.persist_member_graph(write.decisions);
            Ok(AppendOutcome::Applied(sequence))
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
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(OutboxEntry {
                sequence: u64::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
                topic: row.get(1)?,
                payload: row.get(2)?,
            })
        };
        let mut entries: Vec<OutboxEntry> = {
            let mut stmt = self.conn.prepare(sql)?;
            match topic {
                Some(topic) => stmt.query_map(rusqlite::params![topic], map_row)?.collect::<Result<_, _>>()?,
                None => stmt.query_map([], map_row)?.collect::<Result<_, _>>()?,
            }
        };
        for entry in &mut entries {
            if entry.topic == Topic::Commission.as_str() {
                super::commission::overlay_recorded_projection(&self.conn, &mut entry.payload);
            }
        }
        Ok(entries)
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

    fn delivered_outbox(&mut self, topic: &str) -> rusqlite::Result<Vec<OutboxEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT sequence, topic, payload FROM outbox WHERE delivered = 1 AND topic = ?1 ORDER BY sequence",
        )?;
        stmt.query_map(rusqlite::params![topic], |row| {
            Ok(OutboxEntry {
                sequence: u64::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
                topic: row.get(1)?,
                payload: row.get(2)?,
            })
        })?
        .collect()
    }

    fn redeliver_outbox(&mut self, topic: &str, sequence: u64) -> rusqlite::Result<bool> {
        let sequence = i64::try_from(sequence).unwrap_or(i64::MAX);
        let moved = self.conn.execute(
            "UPDATE outbox SET delivered = 0 WHERE sequence = ?1 AND delivered = 1 AND topic = ?2",
            rusqlite::params![sequence, topic],
        )?;
        Ok(moved > 0)
    }

    fn journal_holds_any(&mut self, keys: &[String]) -> rusqlite::Result<bool> {
        if keys.is_empty() {
            return Ok(false);
        }
        // `idempotency_key` is UNIQUE, so each arm of the `IN` is an index probe.
        let placeholders = repeat_n("?", keys.len()).collect::<Vec<_>>().join(", ");
        self.conn
            .prepare(&format!("SELECT 1 FROM journal WHERE idempotency_key IN ({placeholders}) LIMIT 1"))?
            .exists(rusqlite::params_from_iter(keys))
    }

    fn replay_journal(&mut self) -> rusqlite::Result<Vec<JournalRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT sequence, idempotency_key, event, decisions, decider, decisions_schema, recorded_unix_millis \
             FROM journal ORDER BY sequence",
        )?;
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
                decisions_schema: row.get(5)?,
                recorded_unix_millis: recorded_from_column(row.get(6)?),
            })
        })?;
        rows.collect()
    }

    fn journal_recorded_unix_millis(&mut self) -> rusqlite::Result<Vec<Option<u64>>> {
        let mut stmt = self.conn.prepare("SELECT recorded_unix_millis FROM journal ORDER BY sequence")?;
        let rows = stmt.query_map([], |row| Ok(recorded_from_column(row.get(0)?)))?;
        rows.collect()
    }

    fn list_events(&mut self) -> rusqlite::Result<Vec<Vec<u8>>> {
        let mut stmt = self.conn.prepare("SELECT event FROM journal ORDER BY sequence")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        rows.collect()
    }

    fn append_proof_facts(&mut self, facts: &[ProofFactWrite<'_>]) -> rusqlite::Result<()> {
        if facts.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO proof_facts \
                 (closure_key, test_id, result, host_class, producing_dispatch, producing_bloom) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for fact in facts {
                stmt.execute(rusqlite::params![
                    fact.closure_key,
                    fact.test_id,
                    fact.result,
                    fact.host_class,
                    fact.producing_dispatch,
                    fact.producing_bloom,
                ])?;
            }
        }
        tx.commit()
    }

    fn list_proof_facts(&mut self) -> rusqlite::Result<Vec<ProofFactRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT sequence, closure_key, test_id, result, host_class, producing_dispatch, producing_bloom \
             FROM proof_facts ORDER BY sequence",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProofFactRow {
                sequence: row.get(0)?,
                closure_key: row.get(1)?,
                test_id: row.get(2)?,
                result: row.get(3)?,
                host_class: row.get(4)?,
                producing_dispatch: row.get(5)?,
                producing_bloom: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    fn clear_metrics(&mut self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "DELETE FROM metric_dispatch;
             DELETE FROM metric_bloom;
             DELETE FROM metric_day;
             DELETE FROM metric_cursor;",
        )
    }

    fn metrics_cursor(&mut self) -> rusqlite::Result<u64> {
        let value = self
            .conn
            .query_row("SELECT through_sequence FROM metric_cursor WHERE id = 0", [], |row| row.get::<_, i64>(0));
        match value {
            Ok(sequence) => Ok(u64::try_from(sequence).unwrap_or_default()),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(error) => Err(error),
        }
    }

    fn persist_metrics(&mut self, ledger: &MetricsLedger) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        for row in ledger.dispatch_rows() {
            let payload = encode_metric(&row)?;
            // Preserve a host nonce the evidence join already wrote. A rebuild
            // that put the fold id back would un-join every completed attempt.
            tx.execute(
                "INSERT INTO metric_dispatch (id, nonce, sequence, bloom, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET \
                 sequence = excluded.sequence, bloom = excluded.bloom, payload = excluded.payload",
                rusqlite::params![
                    row.id,
                    row.id,
                    i64::try_from(row.sequence).unwrap_or(i64::MAX),
                    row.bloom.0.as_bytes().as_slice(),
                    payload,
                ],
            )?;
        }
        for row in ledger.bloom_rows() {
            let payload = encode_metric(&row)?;
            tx.execute(
                "INSERT OR REPLACE INTO metric_bloom (bloom, seal_sequence, payload) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    row.bloom.0.as_bytes().as_slice(),
                    i64::try_from(row.seal_sequence).unwrap_or(i64::MAX),
                    payload,
                ],
            )?;
        }
        for row in ledger.day_rows() {
            let payload = encode_metric(&row)?;
            tx.execute(
                "INSERT OR REPLACE INTO metric_day (label, payload) VALUES (?1, ?2)",
                rusqlite::params![row.label, payload],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO metric_cursor (id, through_sequence) VALUES (0, ?1)",
            rusqlite::params![i64::try_from(ledger.through_sequence()).unwrap_or(i64::MAX)],
        )?;
        tx.commit()
    }

    fn fold_metrics_from_journal(&mut self) -> rusqlite::Result<MetricsLedger> {
        let configs = resolved_configs(self)?;
        let records = self.replay_journal()?;
        let mut ledger = MetricsLedger::default();
        self.clear_metrics()?;
        for record in &records {
            let event: Event = from_bytes(&record.event).map_err(|error| {
                rusqlite::Error::SqliteFailure(
                    SqliteFfiError::new(SQLITE_ERROR),
                    Some(format!("metrics fold: event {} did not decode: {error}", record.sequence)),
                )
            })?;
            let decisions =
                decode_recorded_decisions(&record.decisions, record.decisions_schema.as_deref()).map_err(|error| {
                    rusqlite::Error::SqliteFailure(
                        SqliteFfiError::new(SQLITE_ERROR),
                        Some(format!("metrics fold: record {} {error}", record.sequence)),
                    )
                })?;
            ledger.observe(record.sequence, &event, &decisions, &configs, record.recorded_unix_millis);
        }
        self.persist_metrics(&ledger)?;
        Ok(ledger)
    }

    fn metric_dispatch_payloads(&mut self) -> rusqlite::Result<Vec<Vec<u8>>> {
        let mut stmt = self.conn.prepare("SELECT payload FROM metric_dispatch ORDER BY sequence, id")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect()
    }

    fn record_metric_evidence(
        &mut self,
        nonce: &str,
        session_reuse_arm: Option<&str>,
        session_reuse_saved_micro_usd: Option<u64>,
        peak_resident_bytes: Option<u64>,
        calls_json: Option<&str>,
    ) -> rusqlite::Result<()> {
        let saved = session_reuse_saved_micro_usd.map(|value| i64::try_from(value).unwrap_or(i64::MAX));
        let peak = peak_resident_bytes.map(|value| i64::try_from(value).unwrap_or(i64::MAX));
        let updated = self.conn.execute(
            "UPDATE metric_dispatch SET session_reuse_arm = ?2, session_reuse_saved_micro_usd = ?3, \
             peak_resident_bytes = ?4, calls_json = ?5, nonce = ?1 \
             WHERE nonce = ?1 OR id = ?1",
            rusqlite::params![nonce, session_reuse_arm, saved, peak, calls_json],
        )?;
        if updated > 0 {
            return Ok(());
        }
        // persist_metrics keys the row by fold identity, not the host nonce.
        // Join through the still-outstanding order's (bloom, displayed) pair.
        let Some(order) = self.lookup_order(nonce)? else {
            return Ok(());
        };
        let mut stmt = self.conn.prepare("SELECT id, payload FROM metric_dispatch WHERE bloom = ?1")?;
        let rows = stmt.query_map(rusqlite::params![&order.bloom], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut matched = None;
        for row in rows {
            let (id, payload) = row?;
            let Ok(dispatch) = from_bytes::<MetricDispatch>(&payload) else {
                continue;
            };
            if dispatch.displayed.as_bytes() == order.displayed_digest.as_slice() {
                matched = Some(id);
                break;
            }
        }
        drop(stmt);
        if let Some(id) = matched {
            self.conn.execute(
                "UPDATE metric_dispatch SET session_reuse_arm = ?2, session_reuse_saved_micro_usd = ?3, \
                 peak_resident_bytes = ?4, calls_json = ?5, nonce = ?1 \
                 WHERE id = ?6",
                rusqlite::params![nonce, session_reuse_arm, saved, peak, calls_json, id],
            )?;
        }
        Ok(())
    }

    fn list_bloom_dispatch_rollup(&mut self, bloom: &[u8]) -> rusqlite::Result<Vec<BloomDispatchRollup>> {
        let _ = self.ensure_metrics();
        let mut stmt = self
            .conn
            .prepare("SELECT nonce, sequence, payload FROM metric_dispatch WHERE bloom = ?1 ORDER BY sequence, id")?;
        let rows = stmt.query_map(rusqlite::params![bloom], |row| {
            Ok(BloomDispatchRollup {
                nonce: row.get(0)?,
                sequence: u64::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                payload: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    fn list_bloom_dispatch_live(&mut self, bloom: &[u8]) -> rusqlite::Result<Vec<BloomDispatchLive>> {
        let mut stmt = self.conn.prepare(
            "SELECT nonce, workpiece, stage, displayed_digest FROM outstanding_orders \
             WHERE bloom = ?1 ORDER BY nonce",
        )?;
        let rows = stmt.query_map(rusqlite::params![bloom], |row| {
            Ok(BloomDispatchLive {
                nonce: row.get(0)?,
                workpiece: row.get(1)?,
                stage: row.get(2)?,
                displayed: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    fn lookup_named_dispatch(&mut self, nonce: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        if let Some(bloom) = self.lookup_dispatch_owner(nonce)? {
            return Ok(Some(bloom));
        }
        if let Some(order) = self.lookup_order(nonce)? {
            return Ok(Some(order.bloom));
        }
        let mut stmt = self.conn.prepare("SELECT bloom FROM metric_dispatch WHERE nonce = ?1")?;
        let mut rows = stmt.query_map(rusqlite::params![nonce], |row| row.get(0))?;
        rows.next().transpose()
    }

    fn ensure_metrics(&mut self) -> rusqlite::Result<()> {
        let cursor = self.metrics_cursor()?;
        let latest =
            self.conn.query_row("SELECT COALESCE(MAX(sequence), 0) FROM journal", [], |row| row.get::<_, i64>(0))?;
        let latest = u64::try_from(latest).unwrap_or_default();
        if latest > cursor {
            self.fold_metrics_from_journal()?;
        }
        Ok(())
    }
}

impl SqliteStore {
    fn persist_member_graph(&mut self, decisions: &[u8]) {
        let Ok(decoded) = decode_recorded_decisions(decisions, Some(DECISIONS_SCHEMA)) else {
            return;
        };
        for effect in decoded.effects {
            let Decision::RecordMemberDependencies { bloom, edges } = effect else {
                continue;
            };
            let digest = bloom.0;
            if self
                .conn
                .execute(
                    "DELETE FROM member_dependency WHERE bloom = ?1",
                    rusqlite::params![digest.as_bytes().as_slice()],
                )
                .is_err()
            {
                return;
            }
            for edge in edges {
                let _ = self.conn.execute(
                    "INSERT INTO member_dependency (bloom, member, depends_on) VALUES (?1, ?2, ?3)",
                    rusqlite::params![digest.as_bytes().as_slice(), edge.member.0, edge.depends_on.0],
                );
            }
        }
    }

    fn upsert_metrics_from_write(
        &mut self,
        sequence: u64,
        write: &JournalWrite<'_>,
        envelope: Option<u64>,
    ) -> rusqlite::Result<()> {
        let Ok(event) = from_bytes::<Event>(write.event) else {
            return Ok(());
        };
        let Ok(decisions) = decode_recorded_decisions(write.decisions, Some(DECISIONS_SCHEMA)) else {
            return Ok(());
        };
        let configs = resolved_configs(self)?;
        let mut ledger = MetricsLedger::default();
        ledger.observe(sequence, &event, &decisions, &configs, envelope);
        for row in ledger.dispatch_rows() {
            let payload = encode_metric(&row)?;
            self.conn.execute(
                "INSERT INTO metric_dispatch (id, nonce, sequence, bloom, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET \
                 sequence = excluded.sequence, bloom = excluded.bloom, payload = excluded.payload",
                rusqlite::params![
                    row.id,
                    row.id,
                    i64::try_from(row.sequence).unwrap_or(i64::MAX),
                    row.bloom.0.as_bytes().as_slice(),
                    payload,
                ],
            )?;
        }
        let through = i64::try_from(sequence).unwrap_or(i64::MAX);
        self.conn.execute(
            "INSERT INTO metric_cursor (id, through_sequence) VALUES (0, ?1) \
             ON CONFLICT(id) DO UPDATE SET through_sequence = MAX(through_sequence, excluded.through_sequence)",
            rusqlite::params![through],
        )?;
        Ok(())
    }
}

fn encode_metric<T: serde::Serialize>(value: &T) -> rusqlite::Result<Vec<u8>> {
    to_vec(value).map_err(|error| {
        rusqlite::Error::SqliteFailure(
            SqliteFfiError::new(SQLITE_ERROR),
            Some(format!("metrics encode failed: {error}")),
        )
    })
}

fn resolved_configs(store: &mut SqliteStore) -> rusqlite::Result<aether_bloomery::ResolvedConfigs> {
    let mut configs = aether_bloomery::ResolvedConfigs::default();
    for record in store.load_configs()? {
        let Some(address) = aether_bloomery::Digest::from_slice(&record.digest) else {
            continue;
        };
        configs.insert(address, record.kind, record.bytes);
    }
    Ok(configs)
}

/// Runtime state for [`StoreCapability`]: the one durable backend the
/// dispatcher owns.
pub struct StoreCapabilityState {
    backend: SqliteStore,
}

impl StoreCapabilityState {
    /// Build state over an explicit store — the seam the handler tests drive.
    #[must_use]
    pub fn new(backend: SqliteStore) -> Self {
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
        Ok(StoreCapabilityState { backend: store })
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

    #[handler::single]
    fn on_page_journal(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: PageJournal) -> PageJournalResult {
        let PageJournal { bloom, from_sequence, limit, descending, notice } = mail;
        match state.backend.replay_journal() {
            Ok(records) => PageJournalResult::Ok { records, bloom, from_sequence, limit, descending, notice },
            Err(error) => PageJournalResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_list_bloom_dispatches(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ListBloomDispatches,
    ) -> ListBloomDispatchesResult {
        let ListBloomDispatches { bloom } = mail;
        let rollup = match state.backend.list_bloom_dispatch_rollup(&bloom) {
            Ok(rollup) => rollup,
            Err(error) => return ListBloomDispatchesResult::Err { error: error.to_string() },
        };
        match state.backend.list_bloom_dispatch_live(&bloom) {
            Ok(outstanding) => ListBloomDispatchesResult::Ok { rollup, outstanding },
            Err(error) => ListBloomDispatchesResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_lookup_dispatch(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: LookupDispatch,
    ) -> LookupDispatchResult {
        let LookupDispatch { nonce } = mail;
        for nonce in nonce_spellings(&nonce) {
            match state.backend.lookup_named_dispatch(&nonce) {
                Ok(Some(bloom)) => return LookupDispatchResult::Ok { nonce, bloom },
                Ok(None) => {}
                Err(error) => return LookupDispatchResult::Err { error: error.to_string() },
            }
        }
        LookupDispatchResult::NotFound
    }

    #[handler::single]
    fn on_create_commission(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CreateCommission,
    ) -> CreateCommissionResult {
        let CreateCommission { id, intent } = mail;
        let intent: Statement = match from_bytes(&intent) {
            Ok(intent) => intent,
            Err(error) => return CreateCommissionResult::Err { error: error.to_string() },
        };
        match state.backend.create(&WorkpieceId(id.clone()), &intent) {
            Ok(digest) => CreateCommissionResult::Ok { id, digest: digest.as_bytes().to_vec() },
            Err(CommissionError::DuplicateCommission(id)) => CreateCommissionResult::Duplicate { id },
            Err(error) => CreateCommissionResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_write_scope_revision(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: WriteScopeRevision,
    ) -> WriteScopeRevisionResult {
        let WriteScopeRevision { canonical } = mail;
        let revision = match ScopeRevision::from_canonical(&canonical) {
            Ok(revision) => revision,
            Err(error) => return write_revision_error(CommissionError::from(error)),
        };
        match state.backend.write_revision(&revision) {
            Ok(digest) => WriteScopeRevisionResult::Ok { digest: digest.as_bytes().to_vec() },
            Err(error) => write_revision_error(error),
        }
    }

    #[handler::single]
    fn on_record_commission_approval(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RecordCommissionApproval,
    ) -> RecordCommissionApprovalResult {
        let statement: Statement = match from_bytes(&mail.statement) {
            Ok(statement) => statement,
            Err(error) => return RecordCommissionApprovalResult::Err { error: error.to_string() },
        };
        match state.backend.record_verified_approval(&WorkpieceId(mail.id.clone()), &statement) {
            Ok(digest) => {
                RecordCommissionApprovalResult::Ok { digest: digest.as_bytes().to_vec(), statement: mail.statement }
            }
            Err(CommissionError::MissingRevision) => RecordCommissionApprovalResult::MissingRevision,
            Err(CommissionError::StaleRevision) => RecordCommissionApprovalResult::Stale,
            Err(error @ (CommissionError::WrongSubject | CommissionError::WrongProvenance)) => {
                RecordCommissionApprovalResult::Refused { error: error.to_string() }
            }
            Err(error) => RecordCommissionApprovalResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_load_commission(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: LoadCommission,
    ) -> LoadCommissionResult {
        match state.backend.load(&WorkpieceId(mail.id.clone())) {
            Ok(None) => LoadCommissionResult::Missing { id: mail.id },
            Ok(Some(view)) => {
                let approvals = match view.head.current_revision {
                    Some(digest) => match state.backend.load_approvals(digest) {
                        Ok(approvals) => approvals,
                        Err(error) => return LoadCommissionResult::Err { error: error.to_string() },
                    },
                    None => Vec::new(),
                };
                let approvals = match encode_statements(&approvals) {
                    Ok(approvals) => approvals,
                    Err(error) => return LoadCommissionResult::Err { error },
                };
                LoadCommissionResult::Ok {
                    id: view.head.id.0,
                    intent: view.head.intent.as_bytes().to_vec(),
                    current_revision: view.head.current_revision.map(|digest| digest.as_bytes().to_vec()),
                    current_ordinal: view.head.current_ordinal,
                    status: view.head.status.as_str().to_owned(),
                    current: view.current.map(|revision| revision.to_canonical()),
                    approvals,
                }
            }
            Err(error) => LoadCommissionResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_list_commissions(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ListCommissions,
    ) -> ListCommissionsResult {
        let ListCommissions { status } = mail;
        let status = match status.as_deref() {
            None => None,
            Some(raw) => match aether_bloomery::CommissionStatus::parse(raw) {
                Some(status) => Some(status),
                None => return ListCommissionsResult::Err { error: format!("unknown commission status `{raw}`") },
            },
        };
        match state.backend.list(status) {
            Ok(heads) => ListCommissionsResult::Ok {
                commissions: heads
                    .into_iter()
                    .map(|head| ListedCommission {
                        id: head.id.0,
                        intent: head.intent.as_bytes().to_vec(),
                        current_revision: head.current_revision.map(|digest| digest.as_bytes().to_vec()),
                        current_ordinal: head.current_ordinal,
                        status: head.status.as_str().to_owned(),
                    })
                    .collect(),
            },
            Err(error) => ListCommissionsResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_cancel_commission(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CancelCommission,
    ) -> CancelCommissionResult {
        let statement: Statement = match from_bytes(&mail.statement) {
            Ok(statement) => statement,
            Err(error) => return CancelCommissionResult::Err { error: error.to_string() },
        };
        match state.backend.cancel(&WorkpieceId(mail.id.clone()), &statement) {
            Ok(digest) => CancelCommissionResult::Ok { id: mail.id, digest: digest.as_bytes().to_vec() },
            Err(CommissionError::MissingCommission(id)) => CancelCommissionResult::Missing { id },
            Err(CommissionError::NotOpen) => CancelCommissionResult::NotOpen,
            Err(CommissionError::WrongSubject) => CancelCommissionResult::WrongSubject,
            Err(error) => CancelCommissionResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_record_commission_projection(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RecordCommissionProjection,
    ) -> RecordCommissionProjectionResult {
        match state.backend.record_projection(&WorkpieceId(mail.id.clone()), mail.issue_number) {
            Ok(()) => RecordCommissionProjectionResult::Ok { id: mail.id, issue_number: mail.issue_number },
            Err(CommissionError::MissingCommission(id)) => RecordCommissionProjectionResult::Missing { id },
            Err(error) => RecordCommissionProjectionResult::Err { error: error.to_string() },
        }
    }
}

fn write_revision_error(error: CommissionError) -> WriteScopeRevisionResult {
    match error {
        CommissionError::MissingCommission(id) => WriteScopeRevisionResult::Missing { id },
        CommissionError::StaleRevision => WriteScopeRevisionResult::Stale,
        CommissionError::DuplicateRevision => WriteScopeRevisionResult::Duplicate,
        CommissionError::OrdinalViolation { expected } => WriteScopeRevisionResult::Ordinal { expected },
        CommissionError::UnsupportedSchema(schema) => WriteScopeRevisionResult::UnsupportedSchema { schema },
        CommissionError::MalformedCanonical => WriteScopeRevisionResult::Malformed,
        error => WriteScopeRevisionResult::Err { error: error.to_string() },
    }
}

fn encode_statements(statements: &[Statement]) -> Result<Vec<Vec<u8>>, String> {
    statements.iter().map(|statement| to_vec(statement).map_err(|error| error.to_string())).collect()
}

/// The nonce as given, then the `dispatch-` / `redispatch-` alternate if either
/// prefix is present — so a caller can name either spelling.
fn nonce_spellings(nonce: &str) -> Vec<String> {
    let mut spellings = vec![nonce.to_owned()];
    if let Some(rest) = nonce.strip_prefix("dispatch-") {
        spellings.push(format!("redispatch-{rest}"));
    } else if let Some(rest) = nonce.strip_prefix("redispatch-") {
        spellings.push(format!("dispatch-{rest}"));
    }
    spellings
}
