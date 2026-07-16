//! Evidence intake: the return path of ADR-0149 migration step 2 (issue #3502).
//!
//! Migration step 2 dispatches fully-resolved work orders to a zero-secret
//! worker lane; the workers upload evidence bytes. This module is the side that
//! *accepts* that evidence — under two hard constraints:
//!
//! - **Trust.** A worker runs untrusted and can lie, so the broker admits an
//!   upload only when its idempotency nonce names a real outstanding order
//!   **and** its bound digest equals the digest that order displayed. A
//!   fabricated or replayed upload is refused, and even an accepted claim is an
//!   *untrusted* claim the reducer re-checks (`reduce_integrate` re-verifies
//!   `claim.evidence.validates(&claim.candidate)`), never a state advance by
//!   itself — the trust boundary is defended twice.
//! - **Reachability.** The host sits behind NAT, so intake is pull-based: it
//!   polls run status and streams uploaded evidence through the existing
//!   [`ExecutorShell`] surface (`inspect` / `stream_evidence`), never a webhook.
//!
//! # The registry, and the dependency direction
//!
//! The nonce → reducer-context linkage is a **host-side dispatch-record**, not a
//! field on the portable core [`WorkOrder`]: ADR-0149 §The line defines a work
//! order as a portable unit blind to which bloom dispatched it, so the bloom /
//! workpiece / scope-revision / candidate context rides the persisted
//! [`OutstandingOrder`](crate::store::OutstandingOrder) registry the host writes
//! at dispatch ([`record_dispatch`]) and reads/consumes at intake
//! ([`admit_uploaded`]). The core order stays `{ transformation, nonce }`.
//!
//! This slice owns the registry plus its write API and the read side; the
//! production dispatch trigger that calls the write when the reducer emits a
//! work-order dispatch decision is #3505's wiring. So the direction is
//! **#3505 → #3502**: #3505 will call [`record_dispatch`] / [`dispatch_and_record`]
//! when it dispatches. This slice drives the write API directly in its own
//! tests; the production loop closes when #3505 wires dispatch to it.
//!
//! [`run_intake_cycle`] is the substantive pull primitive — inspect → stream →
//! broker → admit — that a production loop drives. The *periodic scheduler* that
//! calls it on an interval (and the ADR-0090 poll-interval `Config` knob that
//! paces it) land with that production loop rather than here: until #3505 wires
//! dispatch, there are no tracked runs to poll, so a live timer would poll an
//! empty handle set and an interval knob would have no reader.

use std::error::Error;
use std::fmt;

use aether_bloomery::{
    Admit, BloomId, Digest, Event, EvidenceRef, ExecutionStatus, Fact, IdempotencyKey, Nonce, ResolutionClaim,
    WorkHandle, WorkOrder, WorkpieceId,
};
use aether_bloomery_github::{ExecutorError, InwardError, StageResult, StageVerdict, normalize_stage_result};
use aether_data::wire::{Error as WireError, to_vec};

use super::executor::ExecutorShell;
use crate::store::{OutstandingOrder, RecordOutcome, StoreBackend};

/// A work order's reducer context, captured host-side at dispatch time — the
/// typed form of an [`OutstandingOrder`](crate::store::OutstandingOrder) registry
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
}

impl DispatchRecord {
    fn to_stored(&self) -> OutstandingOrder {
        OutstandingOrder {
            nonce: self.nonce.0.clone(),
            bloom: self.bloom.0.as_bytes().to_vec(),
            workpiece: self.workpiece.0.clone(),
            scope_revision: self.scope_revision.as_bytes().to_vec(),
            candidate: self.candidate.as_bytes().to_vec(),
            displayed_digest: self.displayed_digest.as_bytes().to_vec(),
        }
    }

    fn from_stored(order: &OutstandingOrder) -> Option<Self> {
        Some(Self {
            nonce: Nonce(order.nonce.clone()),
            bloom: BloomId(digest_from_slice(&order.bloom)?),
            workpiece: WorkpieceId(order.workpiece.clone()),
            scope_revision: digest_from_slice(&order.scope_revision)?,
            candidate: digest_from_slice(&order.candidate)?,
            displayed_digest: digest_from_slice(&order.displayed_digest)?,
        })
    }
}

/// Reconstruct a [`Digest`] from stored bytes — `None` when the column is not
/// exactly 32 bytes (a corrupt row, never a well-formed one).
fn digest_from_slice(bytes: &[u8]) -> Option<Digest> {
    <[u8; 32]>::try_from(bytes).ok().map(Digest::from_bytes)
}

/// What a worker uploaded as an attempt result: the nonce it claims to answer,
/// the digest its evidence is about (what the observation *claims*), the
/// verdict, and the supporting detail artifact. The broker checks the claimed
/// `subject` against the digest the matched order displayed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UploadedEvidence {
    /// The nonce the upload claims to answer.
    pub nonce: Nonce,
    /// The digest the upload claims its evidence is about.
    pub subject: Digest,
    /// The verdict the upload carries.
    pub verdict: StageVerdict,
    /// The supporting artifact (the check output, the review record).
    pub detail: Digest,
}

/// Why the broker refused an upload without touching the reducer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IntakeRefusal {
    /// No outstanding order names this nonce — the upload was fabricated, or its
    /// order was already consumed (a replay).
    UnknownNonce(Nonce),
    /// The order's stored row is corrupt (a digest column is not 32 bytes).
    CorruptOrder(Nonce),
    /// The upload's bound digest is not the one the order displayed — evidence
    /// never validates a digest it does not name.
    DigestMismatch {
        /// The digest the matched order displayed.
        displayed: Digest,
        /// The digest the upload actually claimed.
        claimed: Digest,
    },
}

/// An accepted attempt result: the reducer [`Event`] the upload normalized to
/// (a [`Fact::Integrate`]) and the [`Admit`] wire payload for #3497's
/// `aether.bloomery.admit` ingress.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Admission {
    /// The `aether.bloomery.admit` payload to send to the control core.
    pub admit: Admit,
    /// The decoded event the admit carries — a [`Fact::Integrate`].
    pub event: Event,
}

/// The broker's verdict on one upload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdmitDecision {
    /// The upload's provenance and binding both hold — the order was consumed
    /// and the attempt result is ready to admit. Boxed: an [`Admission`] carries
    /// a whole reducer [`Event`], far larger than the refusal variant.
    Admitted(Box<Admission>),
    /// The upload was refused; the reducer is untouched and the order (on a
    /// digest mismatch) stays live for the honest worker.
    Refused(IntakeRefusal),
}

/// A failure that is neither a clean accept nor a clean refuse — the durable
/// store faulted, or an event that should always encode did not.
#[derive(Debug)]
pub enum IntakeError {
    /// The `SQLite` registry read/consume faulted.
    Store(rusqlite::Error),
    /// Encoding the admitted event to wire bytes failed.
    Encode(WireError),
}

impl fmt::Display for IntakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "evidence intake store error: {error}"),
            Self::Encode(error) => write!(f, "evidence intake event encode error: {error}"),
        }
    }
}

impl Error for IntakeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Encode(error) => Some(error),
        }
    }
}

impl From<rusqlite::Error> for IntakeError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl From<WireError> for IntakeError {
    fn from(error: WireError) -> Self {
        Self::Encode(error)
    }
}

/// Record an outstanding order's reducer context at dispatch time — the
/// registry write side (#3502). Idempotent on the nonce.
///
/// # Errors
/// The durable store faulted.
pub fn record_dispatch(store: &mut dyn StoreBackend, record: &DispatchRecord) -> rusqlite::Result<RecordOutcome> {
    store.record_order(&record.to_stored())
}

/// A dispatch that both submitted and recorded its context, or the step that
/// failed.
#[derive(Debug)]
pub enum DispatchError {
    /// The executor refused or could not reach the dispatch surface.
    Submit(ExecutorError),
    /// The registry write faulted after a successful submit.
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

/// Submit a work order through the executor shell and record its outstanding
/// reducer context, in one host step (#3502). Submits first, so a submit that
/// fails records nothing; the registry row is written only for a dispatch that
/// actually reached the worker lane.
///
/// # Errors
/// [`DispatchError::Submit`] if the executor refused the dispatch, or
/// [`DispatchError::Store`] if the registry write faulted after submit.
pub fn dispatch_and_record(
    shell: &ExecutorShell,
    store: &mut dyn StoreBackend,
    order: &WorkOrder,
    record: &DispatchRecord,
) -> Result<WorkHandle, DispatchError> {
    let handle = shell.submit(order).map_err(DispatchError::Submit)?;
    record_dispatch(store, record).map_err(DispatchError::Store)?;
    Ok(handle)
}

/// The broker accept-gate + normalize → admit (#3502, the trust boundary).
///
/// Look the upload's nonce up in the outstanding-order registry; admit only when
/// the nonce names a live order **and** the evidence's bound digest is the one
/// the order displayed. On accept the order is consumed (so a replayed nonce
/// refuses) and the stage result normalizes — through the shape-only
/// [`normalize_stage_result`] — into an [`Evidence`](aether_bloomery::Evidence)
/// bound to the displayed digest, wrapped in a [`ResolutionClaim`] built from the
/// matched row, and carried as a [`Fact::Integrate`]. A mismatch on either axis
/// refuses without touching the reducer.
///
/// # Errors
/// [`IntakeError::Store`] if the registry read/consume faulted, or
/// [`IntakeError::Encode`] if the admitted event failed to wire-encode.
pub fn admit_uploaded(store: &mut dyn StoreBackend, upload: &UploadedEvidence) -> Result<AdmitDecision, IntakeError> {
    let Some(stored) = store.lookup_order(&upload.nonce.0)? else {
        // Fabricated, or the order was already consumed (a replay).
        return Ok(AdmitDecision::Refused(IntakeRefusal::UnknownNonce(upload.nonce.clone())));
    };
    let Some(record) = DispatchRecord::from_stored(&stored) else {
        return Ok(AdmitDecision::Refused(IntakeRefusal::CorruptOrder(upload.nonce.clone())));
    };
    let observed = StageResult { subject: upload.subject, verdict: upload.verdict, detail: upload.detail };
    let evidence = match normalize_stage_result(&record.displayed_digest, &observed) {
        Ok(evidence) => evidence,
        // A mismatch is a lie about which digest the evidence names; refuse and
        // leave the order live so the honest worker can still deliver.
        Err(InwardError::DigestMismatch { displayed, claimed }) => {
            return Ok(AdmitDecision::Refused(IntakeRefusal::DigestMismatch { displayed, claimed }));
        }
    };
    // Provenance (a real outstanding order) and binding (the displayed digest)
    // both hold. Build the whole admission — including the fallible event encode
    // — *before* consuming the order: consuming first and then failing the encode
    // would lose the evidence with the nonce already spent and no retry.
    let claim = ResolutionClaim {
        workpiece: record.workpiece.clone(),
        scope_revision: record.scope_revision,
        candidate: record.candidate,
        evidence,
    };
    let event = Event {
        idempotency_key: IdempotencyKey(format!("aether.bloomery.integrate:{}", record.nonce.0)),
        fact: Fact::Integrate { bloom: record.bloom, claim },
    };
    let admit = Admit { event: to_vec(&event)? };
    // Consume-once, only now that the admission is fully constructed. A lost race
    // to consumption between the lookup and here reads as a replay.
    if !store.consume_order(&record.nonce.0)? {
        return Ok(AdmitDecision::Refused(IntakeRefusal::UnknownNonce(upload.nonce.clone())));
    }
    Ok(AdmitDecision::Admitted(Box::new(Admission { admit, event })))
}

/// The seam that turns a pulled [`EvidenceRef`] into the attempt result a worker
/// uploaded. The executor port surfaces only references (name / id / size);
/// decoding the referenced bytes into an [`UploadedEvidence`] is this
/// evidence-return path's job (the `EvidenceRef` doc defers "decoding the
/// referenced bytes into a reducer attempt-result" here). The production
/// GitHub-artifact fetch + decode lands with the dispatch wiring (#3505); host
/// tests implement this directly to drive claims.
pub trait EvidenceClaims {
    /// The attempt result the referenced upload carries, or `None` when the
    /// reference is not a decodable attempt result.
    fn claim_for(&self, evidence: &EvidenceRef) -> Option<UploadedEvidence>;
}

/// Where an admitted attempt result goes — #3497's `aether.bloomery.admit`
/// ingress (addressing the `aether.bloomery.control` actor by name) in
/// production, a collector in tests.
pub trait AdmitSink {
    /// Consume one admitted attempt result.
    fn admit(&mut self, admission: Admission);
}

/// What one [`run_intake_cycle`] observed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CycleReport {
    /// Completed runs whose evidence was streamed.
    pub completed: u32,
    /// Uploads admitted to the reducer.
    pub admitted: u32,
    /// Uploads refused by the broker.
    pub refused: u32,
}

/// A fault during an intake cycle.
#[derive(Debug)]
pub enum CycleError {
    /// Inspecting a run faulted.
    Inspect(ExecutorError),
    /// Streaming a run's evidence faulted.
    Stream(ExecutorError),
    /// The broker faulted on the registry or an event encode.
    Intake(IntakeError),
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspect(error) => write!(f, "intake inspect failed: {error}"),
            Self::Stream(error) => write!(f, "intake stream_evidence failed: {error}"),
            Self::Intake(error) => write!(f, "intake broker failed: {error}"),
        }
    }
}

impl Error for CycleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inspect(error) | Self::Stream(error) => Some(error),
            Self::Intake(error) => Some(error),
        }
    }
}

/// One intake pull cycle over the tracked handles (#3502): inspect each
/// dispatched run and, on completion, stream its evidence, run each upload
/// through the [`admit_uploaded`] broker, and send every admitted attempt result
/// to the `sink`. A run that is not yet complete is skipped; a refused upload
/// never reaches the sink (and so never touches the reducer).
///
/// # Errors
/// [`CycleError`] if inspecting a run, streaming its evidence, or the broker
/// faulted; a clean broker refusal is counted, not an error.
pub fn run_intake_cycle(
    store: &mut dyn StoreBackend,
    shell: &ExecutorShell,
    handles: &[WorkHandle],
    claims: &dyn EvidenceClaims,
    sink: &mut dyn AdmitSink,
) -> Result<CycleReport, CycleError> {
    let mut report = CycleReport::default();
    for handle in handles {
        let status = shell.inspect(handle).map_err(CycleError::Inspect)?;
        if !matches!(status, ExecutionStatus::Completed { .. }) {
            continue;
        }
        report.completed += 1;
        for reference in shell.stream_evidence(handle).map_err(CycleError::Stream)? {
            let Some(upload) = claims.claim_for(&reference) else {
                continue;
            };
            match admit_uploaded(store, &upload).map_err(CycleError::Intake)? {
                AdmitDecision::Admitted(admission) => {
                    report.admitted += 1;
                    sink.admit(*admission);
                }
                AdmitDecision::Refused(_) => report.refused += 1,
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
