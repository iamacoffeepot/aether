//! Dispatch-record persistence: the outstanding-order registry write side.

use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fmt, io};

use aether_bloomery::{
    AgentProfile, BloomId, ConfigRegistry, Digest, Nonce, StageId, Transformation, WorkHandle, WorkOrder, WorkpieceId,
};
use aether_bloomery_github::{ExecutorError, GithubError};
use aether_data::wire::to_vec;

use crate::artifacts::{ArtifactsCapabilityState, PutResult};
use crate::bloomery::executor::{ExecutorPort, ExecutorPortError, LocalExecutorError, Settled};
use crate::bloomery::provenance::{ProvenanceRefusal, admit_model_dispatch, gated, journal_refusal};
use crate::store::{OrderLifecycle, OutstandingOrder, RecordOutcome, StoreBackend};

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
    /// Exact authorized instruction-bundle bytes a model lane consumes. `None`
    /// on a mechanical lane. Not persisted: the bundle lives in the config store
    /// under the sealed pin; this field is the host-to-executor hand-off.
    pub instruction_bundle: Option<Vec<u8>>,
    /// Content address of the assembled prompt-manifest bytes retained as
    /// attempt evidence. Persisted on the order row.
    pub prompt_manifest: Option<Digest>,
}

impl DispatchRecord {
    /// The [`WorkOrder`] this record dispatches: the record *is* the order plus
    /// its reducer context, so the two cannot name different nonces or lanes.
    #[must_use]
    pub fn to_order(&self) -> WorkOrder {
        WorkOrder {
            transformation: self.transformation.clone(),
            nonce: self.nonce.clone(),
            instruction_bundle: self.instruction_bundle.clone(),
            prompt_manifest: self.prompt_manifest,
        }
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
            lifecycle: OrderLifecycle::Submitting,
            prompt_manifest: self.prompt_manifest.map(|digest| digest.as_bytes().to_vec()),
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
    /// The instruction-provenance gate refused the dispatch before anything was
    /// recorded or submitted (ADR-0149, ADR-0214). Nothing reached a worker and
    /// no order exists.
    Provenance(ProvenanceRefusal),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Submit(error) => write!(f, "work-order submit failed: {error}"),
            Self::Store(error) => write!(f, "dispatch-record write failed: {error}"),
            Self::Provenance(refusal) => write!(f, "instruction provenance refused the dispatch: {refusal}"),
        }
    }
}

impl Error for DispatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Submit(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Provenance(refusal) => Some(refusal),
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
    ///
    /// A provenance refusal answers for itself: an unauthorized or unresolvable
    /// instruction bundle is immutable content and an immutable authorization
    /// set, so only the store-fault arm of it can clear on a retry.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::Provenance(refusal) => refusal.is_permanent(),
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
/// executor port, in one host step (#3502).
///
/// **Records first.** The local executor starts the lane inside `submit` and
/// resolves the order's (bloom, workpiece, stage) from this very registry row
/// to decide session reuse. Submitting first therefore hands the executor a
/// nonce it cannot resolve: every journaled resume — a refine's construct
/// session, a dependent construct's predecessor session — reads as "no such
/// order" and silently falls through to the pool, where the member's own grown
/// context is refused on the context cap.
///
/// The row is written as [`OrderLifecycle::Submitting`]. Readers that mean
/// "waiting on a run" ignore it, so handing `submit` to a worker no longer
/// publishes a reservation as a dispatch (#5564). When the port answers with a
/// handle the row is promoted to [`OrderLifecycle::Submitted`]; a submit that
/// then fails removes the row it wrote, so a dispatch that never reached the
/// worker lane leaves no registry entry behind. [`Settled::InFlight`] leaves
/// the submitting row in place and the outbox entry unacked — the same "not
/// asked yet" reading every other offloaded call uses. After a restart there
/// is no live worker, the outbox is still unacked, and the drain re-drives
/// under the same nonce: `INSERT OR IGNORE` reuses the submitting row rather
/// than inserting a second one.
///
/// `now_unix_millis` is the clock reading the order's ADR-0177 deadline is
/// computed from — taken by the caller once per tick, and injected rather than
/// read here so a scenario can place a dispatch anywhere relative to it. The
/// deadline starts at the *record*, which is now also the earlier of the two
/// steps: the sealed allowance covers the submit as well.
///
/// **Gates first.** Every model lane goes through here, which is what makes this
/// the one place instruction provenance can be enforced *by construction*
/// (ADR-0149 §The value vocabulary, ADR-0214): a dispatch whose pinned
/// instruction bundle is missing, altered, incomplete, or unauthorized is refused
/// before an order row exists and before anything reaches a worker, and the
/// refusal is parked for the reactor to journal as the host fault it is. A future
/// dispatch site cannot acquire a model lane without passing here, so the gate
/// cannot be forgotten at a call site. The gate is synchronous — it reads this
/// process's own store — so it decides before the submit is handed out and a
/// refused dispatch never reaches a worker at all.
///
/// # Errors
/// [`DispatchError::Provenance`] if the instruction-provenance gate refused
/// (nothing was recorded or submitted), [`DispatchError::Store`] if the registry
/// write faulted (nothing was submitted), or [`DispatchError::Submit`] if the
/// executor refused the dispatch (the registry row is removed again first).
pub fn dispatch_and_record(
    port: &dyn ExecutorPort,
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    record: &DispatchRecord,
    now_unix_millis: u64,
) -> Result<Settled<WorkHandle>, DispatchError> {
    let mut record = record.clone();
    if gated(&record.transformation.command) {
        match admit_model_dispatch(store, &record) {
            Err(refusal) => {
                journal_refusal(store, &record, &refusal);
                return Err(DispatchError::Provenance(refusal));
            }
            Ok(admitted) => {
                if let Ok(manifest_bytes) = to_vec(&admitted.manifest) {
                    let address = Digest::of_wire_bytes(&manifest_bytes);
                    if let Some(artifacts) = artifacts {
                        let parents = vec![admitted.bundle_address.to_hex(), record.displayed_digest.to_hex()];
                        if let PutResult::Err { error } = artifacts.put(&manifest_bytes, &parents) {
                            tracing::warn!(
                                target: "aether_chassis_bloomery::provenance",
                                nonce = %record.nonce.0,
                                ?error,
                                "assembled prompt manifest was not retained in the artifact store",
                            );
                        }
                    }
                    record.prompt_manifest = Some(address);
                }
                record.instruction_bundle = Some(admitted.bundle_bytes);
            }
        }
    }
    record_dispatch_at(store, &record, now_unix_millis).map_err(DispatchError::Store)?;
    match port.submit(&record.to_order()) {
        Settled::InFlight => Ok(Settled::InFlight),
        Settled::Answered(Ok(handle)) => {
            store.mark_order_submitted(&record.nonce.0).map_err(DispatchError::Store)?;
            Ok(Settled::Answered(handle))
        }
        Settled::Answered(Err(error)) => {
            // Nothing reached the worker lane, so the row describes a dispatch
            // that does not exist; drop it rather than leave the deadline sweep
            // to expire an order no run was ever started for.
            let _ = store.consume_order(&record.nonce.0);
            Err(DispatchError::Submit(error))
        }
    }
}
