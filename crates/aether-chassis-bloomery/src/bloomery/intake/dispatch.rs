//! Dispatch-record persistence: the outstanding-order registry write side.

use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fmt, io};

use aether_bloomery::{
    AgentProfile, BloomId, ConfigRegistry, Digest, Nonce, StageId, Transformation, WorkHandle, WorkOrder, WorkpieceId,
};
use aether_bloomery_github::{ExecutorError, GithubError};
use aether_data::wire::to_vec;

use crate::bloomery::executor::{ExecutorPortError, ExecutorShell, LocalExecutorError};
use crate::store::{OutstandingOrder, RecordOutcome, StoreBackend};

/// The idempotency nonce a drained outbox entry dispatches under.
///
/// A pure function of the entry's outbox sequence, which is what makes a
/// dispatch *addressable from the outbox row alone*: a re-drive of the same
/// entry submits under the same nonce, so it collides with the order already
/// recorded rather than opening a second one, and a boot-time reader can name
/// the order an acked entry produced without holding any of the process state
/// that produced it. Every drain — member line, aggregate review, aggregate
/// verify — mints through here, so the three cannot drift into separate
/// spellings of the one convention the store's `dispatch_owners` and
/// `outstanding_orders` rows are keyed by.
#[must_use]
pub fn dispatch_nonce(sequence: u64) -> Nonce {
    Nonce(format!("dispatch-{sequence}"))
}

/// A work order's reducer context, captured host-side at dispatch time — the
/// typed form of an [`OutstandingOrder`] registry
/// row. The caller (the reducer's dispatch path in production, a test here)
/// supplies every field; the portable core [`WorkOrder`] is unchanged.
///
/// A well-formed record has `candidate == displayed_digest`: the digest Bloomery
/// displayed for the order *is* the candidate the worker's evidence must bind to
/// (`Evidence.subject` binds to the candidate). The registry keeps both fields
/// so the broker binds evidence to the displayed digest while the claim names
/// the candidate; the reducer's re-check is what enforces they agree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DispatchRecord {
    /// The dispatched worker's idempotency nonce.
    pub nonce: Nonce,
    /// The bloom the resolved candidate integrates into.
    pub bloom: BloomId,
    /// The member workpiece this order resolves.
    pub workpiece: WorkpieceId,
    /// The scope revision the candidate was integrated against.
    pub scope_revision: Digest,
    /// The exact candidate digest the evidence must bind to.
    pub candidate: Digest,
    /// The digest Bloomery displayed for this order.
    pub displayed_digest: Digest,
    /// The line stage this order dispatched (#3505). Routes the returning result:
    /// a non-terminal per-member stage (`Construct` / `Verify` / `Refine`) admits
    /// as a `Fact::AttemptCompleted` advancing the member's cursor; the terminal
    /// `Review` admits as a `Fact::Integrate`; a parked outcome as a `Question`.
    pub stage: StageId,
    /// The transformation this order dispatched — the record's half of the
    /// [`WorkOrder`] the executor receives, and the exact lane a parked attempt
    /// is re-dispatched by replaying (#3664).
    pub transformation: Transformation,
    /// The configuration this dispatch runs under (ADR-0174) — the member's
    /// registry layered over the bloom's, as the reducer flattened it. Persisted
    /// with the order for the same reason the transformation is: a parked
    /// attempt re-dispatches by replaying the stored order (#3664), and nothing
    /// else host-side carries it.
    pub configs: ConfigRegistry,
    /// The [`AgentProfile`] the bloom's sealed stage catalog calibrates this
    /// stage at (ADR-0174), resolved by the reducer and carried here for the same
    /// reason `configs` is — a replay cannot reconstruct it, and falling back to
    /// the compiled line would re-dispatch the fleet default for a bloom that
    /// sealed something else.
    pub profile: AgentProfile,
}

impl DispatchRecord {
    /// The [`WorkOrder`] this record dispatches: the record *is* the order plus
    /// its reducer context, so the two cannot name different nonces or lanes.
    #[must_use]
    pub fn to_order(&self) -> WorkOrder {
        WorkOrder { transformation: self.transformation.clone(), nonce: self.nonce.clone() }
    }

    /// Whether this order is the reserved composition workpiece's weave repair.
    ///
    /// `Refine` of the composition, and only that — the same fact the executor
    /// uses to seed and park a weave-repair prompt and the intake broker uses to
    /// decide repair-lap triage (#4959, #5098).
    #[must_use]
    pub fn is_composition_refine(&self) -> bool {
        self.stage == StageId::Refine && self.workpiece.is_composition()
    }

    fn to_stored(&self, deadline_unix_millis: u64) -> OutstandingOrder {
        OutstandingOrder {
            deadline_unix_millis,
            nonce: self.nonce.0.clone(),
            bloom: self.bloom.0.as_bytes().to_vec(),
            workpiece: self.workpiece.0.clone(),
            scope_revision: self.scope_revision.as_bytes().to_vec(),
            candidate: self.candidate.as_bytes().to_vec(),
            displayed_digest: self.displayed_digest.as_bytes().to_vec(),
            // The StageId as its canonical wire bytes — a stable, compact column
            // the intake decodes back on admit (never a hand-rolled int mapping).
            stage: to_vec(&self.stage).unwrap_or_default(),
            // Same convention for the transformation, so a parked attempt's lane
            // survives into `parked_question` for the redispatch to replay.
            transformation: to_vec(&self.transformation).unwrap_or_default(),
            // Same convention again for the sealed configuration the lane runs
            // under, so a replay resolves the same overrides the parked attempt did.
            configs: to_vec(&self.configs).unwrap_or_default(),
            // And again for the sealed profile, so a replayed lane dispatches the
            // agent the bloom's catalog named rather than the compiled line's.
            profile: to_vec(&self.profile).unwrap_or_default(),
        }
    }
}

/// The current wall clock in Unix milliseconds — the one clock a dispatch
/// deadline can be written in and read back after a restart (ADR-0177).
///
/// A clock before the epoch is not a time any deadline arithmetic can use, so it
/// reads as `0`. The order it stamps is then due at `0 + limit`, and the sweep
/// that tests it reads the same unusable clock back as `0` — so such an order
/// does not expire early and does not expire at all: deadline enforcement stands
/// down until the host's clock is usable, rather than terminating work on a
/// number that means nothing.
pub(super) fn now_unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// The absolute deadline an order recorded at `now_unix_millis` runs to, from
/// the sealed limit its transformation carries (ADR-0177).
///
/// Saturating rather than wrapping: `wall_clock_secs` is validated at or below
/// [`ExecutionLimits::MAX_WALL_CLOCK_SECS`](aether_bloomery::ExecutionLimits::MAX_WALL_CLOCK_SECS)
/// so the product is nowhere near the ceiling for any catalog a seal admits, and
/// saturating a hypothetical foreign one at [`u64::MAX`] is the arm a checked
/// conversion would have to pick anyway.
fn deadline_from(record: &DispatchRecord, now_unix_millis: u64) -> u64 {
    now_unix_millis.saturating_add(record.transformation.limits.wall_clock_secs.saturating_mul(1_000))
}

/// Record an outstanding order's reducer context at dispatch time — the
/// registry write side (#3502). Idempotent on the nonce, deadline included: a
/// re-recorded nonce keeps the deadline its first record computed.
///
/// # Errors
/// The durable store faulted.
pub fn record_dispatch(store: &mut dyn StoreBackend, record: &DispatchRecord) -> rusqlite::Result<RecordOutcome> {
    record_dispatch_at(store, record, now_unix_millis())
}

/// [`record_dispatch`] against an explicit clock reading — the seam a test drives
/// so a deadline assertion does not depend on when the suite ran.
///
/// # Errors
/// The durable store faulted.
pub(super) fn record_dispatch_at(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    now_unix_millis: u64,
) -> rusqlite::Result<RecordOutcome> {
    store.record_order(&record.to_stored(deadline_from(record, now_unix_millis)))
}

/// A dispatch that both submitted and recorded its context, or the step that
/// failed.
#[derive(Debug)]
pub enum DispatchError {
    /// The executor refused or could not reach the dispatch surface. The
    /// registry row written just before it has been removed again.
    Submit(ExecutorPortError),
    /// The registry write faulted, so nothing was submitted.
    Store(rusqlite::Error),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Submit(error) => write!(f, "work-order submit failed: {error}"),
            Self::Store(error) => write!(f, "dispatch-record write failed: {error}"),
        }
    }
}

impl Error for DispatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Submit(error) => Some(error),
            Self::Store(error) => Some(error),
        }
    }
}

impl DispatchError {
    /// Whether this fault is permanent — a refusal that will not clear on
    /// retry, so the drain parks the entry instead of re-driving it forever.
    /// Two shapes qualify: a GitHub HTTP 4xx other than the 429 rate-limit, and
    /// a local spawn refused with `E2BIG` — the argv the coordinator composed
    /// exceeds a kernel constant, so the identical re-drive fails identically
    /// (#5161: ten hours of silent five-minute retries, board unwedged). All
    /// else is transient and a re-drive can recover it: a 429, any 5xx,
    /// transport/decode/pagination faults, `NoRunForNonce`, the rest of the
    /// local-lane arm (worktree/io/evidence, other spawn faults), and a
    /// post-submit registry write fault.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::Submit(ExecutorPortError::Actions(ExecutorError::Github(GithubError::Status { status, .. }))) => {
                (400..500).contains(status) && *status != 429
            }
            Self::Submit(ExecutorPortError::Local(LocalExecutorError::Spawn(error))) => {
                error.kind() == io::ErrorKind::ArgumentListTooLong
            }
            _ => false,
        }
    }
}

/// Record a work order's outstanding reducer context and submit it through the
/// executor shell, in one host step (#3502).
///
/// **Records first.** `submit` is synchronous into the local executor, which
/// starts the lane on a free slot inside that call and resolves the order's
/// (bloom, workpiece, stage) from this very registry row to decide session
/// reuse. Submitting first therefore hands the executor a nonce it cannot
/// resolve: every journaled resume — a refine's construct session, a dependent
/// construct's predecessor session — reads as "no such order" and silently
/// falls through to the pool, where the member's own grown context is refused
/// on the context cap. A submit that then fails removes the row it wrote, so a
/// dispatch that never reached the worker lane leaves no registry entry behind.
///
/// `now_unix_millis` is the clock reading the order's ADR-0177 deadline is
/// computed from — taken by the caller once per tick, and injected rather than
/// read here so a scenario can place a dispatch anywhere relative to it. The
/// deadline starts at the *record*, which is now also the earlier of the two
/// steps: the sealed allowance covers the submit as well.
///
/// # Errors
/// [`DispatchError::Store`] if the registry write faulted (nothing was
/// submitted), or [`DispatchError::Submit`] if the executor refused the
/// dispatch (the registry row is removed again first).
pub fn dispatch_and_record(
    shell: &ExecutorShell,
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    now_unix_millis: u64,
) -> Result<WorkHandle, DispatchError> {
    record_dispatch_at(store, record, now_unix_millis).map_err(DispatchError::Store)?;
    shell.submit(&record.to_order()).map_err(|error| {
        // Nothing reached the worker lane, so the row describes a dispatch that
        // does not exist; drop it rather than leave the deadline sweep to expire
        // an order no run was ever started for.
        let _ = store.consume_order(&record.nonce.0);
        DispatchError::Submit(error)
    })
}
