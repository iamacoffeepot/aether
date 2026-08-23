//! The pull-loop cycle: inspect each tracked run and, on completion, stream its
//! evidence through the broker to the sink.

use std::error::Error;
use std::fmt;

use aether_bloomery::{Admit, BackendId, ExecutionStatus, LaneObservation, Nonce, StageVerdict, WorkHandle};
use aether_data::wire::to_vec;

use super::admit::{Admission, AdmitDecision, IntakeError, IntakeRefusal, UploadedEvidence, admit_uploaded};
use super::claims::EvidenceClaims;
use super::dispatch::DispatchRecord;
use crate::artifacts::ArtifactsCapabilityState;
use crate::bloomery::executor::{ExecutorPortError, ExecutorShell};
use crate::bloomery::study::{
    StudyAdmission, StudyAdmitDecision, UploadedStudyRecord, admit_study, study_evidence_event,
};
use crate::store::StoreBackend;

/// Where an admitted attempt result goes — #3497's `aether.bloomery.admit`
/// ingress (addressing the `aether.bloomery.control` actor by name) in
/// production, a collector in tests.
pub trait AdmitSink {
    /// Consume one admitted attempt result.
    fn admit(&mut self, admission: Admission);
}

/// What one [`run_intake_cycle`] observed.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CycleReport {
    /// Completed runs whose evidence was streamed.
    pub completed: u32,
    /// Uploads admitted to the reducer.
    pub admitted: u32,
    /// Uploads refused by the broker.
    pub refused: u32,
    /// Study rows written this cycle (#4679) — one per admitted attempt that
    /// reported a cost. Always at most `completed`, and below it whenever a
    /// harness reported no usage or no artifacts store was configured.
    pub studied: u32,
    /// Handles inspected this cycle that were not yet `Completed`, paired with
    /// their observed status (#3635) — feeds the executor reactor's staleness
    /// sweep so a wedged dispatch's last status is visible in its warn without a
    /// second `inspect` call, and carries a running lane's host-observed
    /// progress timestamp so the heartbeat reaper sees the backend observation
    /// without another `inspect`. Completed-evidence-first ordering is unchanged:
    /// a `Completed` handle is streamed here and never appears in `pending`.
    pub pending: Vec<(Nonce, ExecutionStatus)>,
    /// Handles this cycle never resolved because the backend arm holding them
    /// faulted (#5412) — inspected-and-errored, or skipped after an earlier
    /// handle on the same arm errored.
    ///
    /// Load-bearing to the caller's deadline and silence sweeps, not merely
    /// informational: "still pending" for one of these nonces means "never
    /// asked", and cancelling on that reading destroys a lane that finished
    /// well inside its budget. A nonce here carries no `pending` entry and was
    /// never counted `completed`.
    pub unobserved: Vec<Nonce>,
}

/// A fault that abandons a whole intake cycle.
///
/// Only the broker: it writes the registry every admission consumes, so a store
/// or encode fault under it makes every remaining decision this cycle would
/// take unsound. A *backend* fault is not here — it is isolated to the arm that
/// raised it and reported as [`CycleReport::unobserved`], because one arm's
/// transport says nothing about another's finished work.
#[derive(Debug)]
pub enum CycleError {
    /// The broker faulted on the registry or an event encode.
    Intake(IntakeError),
}

impl fmt::Display for CycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Intake(error) => write!(f, "intake broker failed: {error}"),
        }
    }
}

impl Error for CycleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
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
/// Runs *before* the caller's deadline sweep, and the order is load-bearing
/// (ADR-0177): evidence that arrived at or just before the deadline boundary is
/// admitted normally and consumes its order, so the sweep that follows never
/// sees it and a lane that finished in time is never cancelled for being late to
/// be observed. Only an order still pending after this cycle can expire.
///
/// # Backend isolation (#5412)
///
/// The handles are grouped by the backend arm that will actually answer for
/// them ([`ExecutorShell::backend_for`]), and a fault is isolated to its arm:
/// the fault is logged, the rest of that arm's handles are skipped for this
/// tick, and every other arm is still inspected and still admits. An arm
/// holding none of these handles is not asked at all.
///
/// One flat list cost a whole afternoon of local lane results: a shared-runner
/// API answering `403 API rate limit exceeded` aborted the cycle before it
/// reached the local lane beside it, once a second for twenty-eight minutes,
/// and a reconcile that had finished with a candidate was never admitted. The
/// arms fail independently, so they must be asked independently.
///
/// Skipped handles come back as [`CycleReport::unobserved`], which the caller's
/// sweeps must honour: a nonce there is "never asked", not "still pending".
///
/// # Errors
/// [`CycleError`] if the broker faulted; a clean broker refusal is counted, not
/// an error. A broker fault does abandon the loop — it is the registry every
/// admission consumes.
pub fn run_intake_cycle(
    store: &mut dyn StoreBackend,
    shell: &ExecutorShell,
    handles: &[WorkHandle],
    claims: &dyn EvidenceClaims,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    sink: &mut dyn AdmitSink,
) -> Result<CycleReport, CycleError> {
    let mut report = CycleReport::default();
    let mut artifacts = artifacts;
    let mut faulted: Vec<BackendId> = Vec::new();
    for handle in handles {
        let backend = shell.backend_for(handle);
        if faulted.contains(&backend) {
            report.unobserved.push(handle.nonce.clone());
            continue;
        }
        let status = match shell.inspect(handle) {
            Ok(status) => status,
            Err(error) => {
                skip_backend(&mut report, &mut faulted, backend, handle, &error, "inspect");
                continue;
            }
        };
        if !matches!(status, ExecutionStatus::Completed { .. }) {
            report.pending.push((handle.nonce.clone(), status));
            continue;
        }
        report.completed += 1;
        let references = match shell.stream_evidence(handle) {
            Ok(references) => references,
            Err(error) => {
                skip_backend(&mut report, &mut faulted, backend, handle, &error, "stream_evidence");
                continue;
            }
        };
        for reference in references {
            let Some(upload) = claims.claim_for(&reference) else {
                continue;
            };
            // Study first, and deliberately: `admit_study` matches the order
            // *without* consuming it, while `admit_uploaded` below consumes.
            // Reversed, every study upload would look up an already-spent nonce
            // and refuse as `UnknownNonce` — the lane would be wired and still
            // record nothing, which is the failure this issue exists to end.
            if let Some(admission) = record_cost(store, artifacts.as_deref_mut(), &upload) {
                report.studied += 1;
                admit_study_evidence(sink, &admission, &upload.nonce);
            }
            match admit_uploaded(store, &upload).map_err(CycleError::Intake)? {
                AdmitDecision::Admitted(admission) => {
                    report.admitted += 1;
                    sink.admit(*admission);
                }
                // A scoping run's verdict landed on the commission store's own
                // ledger and there is nothing to hand the control core
                // (ADR-0208, #5304). Counted as admitted because it is: the
                // order was consumed and the verdict reached the coordinator.
                AdmitDecision::Recorded => report.admitted += 1,
                AdmitDecision::Refused(refusal) => {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::intake",
                        nonce = %upload.nonce.0,
                        ?refusal,
                        "attempt evidence refused",
                    );
                    report.refused += 1;
                    // The lane process has already exited (`Completed` above).
                    // DigestMismatch leaves the order live so an honest worker
                    // still in flight can retry; a completed run cannot, and
                    // waiting for it is the stall this cycle used to sit in
                    // (#5332). Recover as a machinery fault so the member
                    // retries or wedges on the sealed budget.
                    if matches!(refusal, IntakeRefusal::DigestMismatch { .. })
                        && let Some(admission) = recover_completed_mismatch(store, &upload)?
                    {
                        report.admitted += 1;
                        sink.admit(*admission);
                    }
                }
            }
        }
    }
    Ok(report)
}

/// Log one arm's port fault, mark the arm faulted for the rest of this cycle,
/// and record `handle` as unobserved.
///
/// The arm is marked rather than merely stepped over so a rate-limited API is
/// asked once per tick instead of once per outstanding handle — retrying the
/// same refusal across a whole tracked list is how the budget was spent in the
/// first place.
fn skip_backend(
    report: &mut CycleReport,
    faulted: &mut Vec<BackendId>,
    backend: BackendId,
    handle: &WorkHandle,
    error: &ExecutorPortError,
    message: &'static str,
) {
    tracing::warn!(
        target: "aether_chassis_bloomery::intake",
        nonce = %handle.nonce.0,
        backend = %backend,
        %error,
        call = message,
        "executor arm faulted; skipping its remaining handles this cycle, other arms admit normally",
    );
    faulted.push(backend);
    report.unobserved.push(handle.nonce.clone());
}

/// A completed run whose evidence bound the wrong digest cannot retry that
/// upload. Admit an executor fault against the order's displayed digest so
/// the member's machinery series moves instead of waiting forever.
fn recover_completed_mismatch(
    store: &mut dyn StoreBackend,
    refused: &UploadedEvidence,
) -> Result<Option<Box<Admission>>, CycleError> {
    let Some(stored) = store.lookup_order(&refused.nonce.0).map_err(|error| CycleError::Intake(error.into()))? else {
        return Ok(None);
    };
    let Some(record) = DispatchRecord::from_stored(&stored) else {
        return Ok(None);
    };
    let fault = UploadedEvidence {
        nonce: refused.nonce.clone(),
        subject: record.displayed_digest,
        verdict: StageVerdict::ExecutorFault,
        detail: refused.detail,
        observation: LaneObservation {
            findings: Some("lane bound evidence to a digest the order did not display".into()),
            ..LaneObservation::default()
        },
    };
    match admit_uploaded(store, &fault).map_err(CycleError::Intake)? {
        AdmitDecision::Admitted(admission) => Ok(Some(admission)),
        // A recovery fault against a scoping run has already been recorded on
        // its ledger; there is no event for the caller to admit.
        AdmitDecision::Recorded | AdmitDecision::Refused(_) => Ok(None),
    }
}

/// Record what one attempt cost, returning the admission when a study row
/// was written (#4679).
///
/// Three shapes yield no row and are not failures: an upload carrying no cost
/// (the harness reported no usage, or the name-only Actions lane produced the
/// reference), a host with no artifacts store configured, and a broker refusal.
///
/// A store or artifact **fault** is also swallowed — logged, never returned. The
/// study lane grades attempts; it does not gate them. Propagating here would let
/// a full disk or a faulted content store abort the intake cycle and strand the
/// verdict admit that follows, trading a missing ledger row for a stalled bloom.
/// That trade is never worth taking, so the ledger is the thing allowed to have
/// a hole, and the hole is loud in the log.
fn record_cost(
    store: &mut dyn StoreBackend,
    artifacts: Option<&mut ArtifactsCapabilityState>,
    upload: &UploadedEvidence,
) -> Option<StudyAdmission> {
    let (Some(cost), Some(artifacts)) = (upload.observation.cost, artifacts) else {
        return None;
    };
    let record = UploadedStudyRecord {
        nonce: upload.nonce.clone(),
        subject: upload.subject,
        cost,
        calls: upload.observation.calls.clone(),
        session_reuse_arm: upload.observation.session_reuse_arm.clone(),
        session_reuse_saved_micro_usd: upload.observation.session_reuse_saved_micro_usd,
        peak_resident_bytes: upload.observation.peak_resident_bytes,
    };

    match admit_study(store, artifacts, &record) {
        Ok(StudyAdmitDecision::Admitted(admission)) => Some(admission),
        Ok(StudyAdmitDecision::Refused(refusal)) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::intake",
                nonce = %upload.nonce.0,
                ?refusal,
                "study record refused; the attempt admits normally but its cost is unrecorded",
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::intake",
                nonce = %upload.nonce.0,
                %error,
                "study record could not be stored; the attempt admits normally but its cost is unrecorded",
            );
            None
        }
    }
}

/// Forward the study artifact as journal evidence so a calibration read can
/// resolve it. A missing hex digest or an encode fault is logged and dropped
/// — the artifact and index row already landed, and the study lane does not
/// gate the verdict that follows.
fn admit_study_evidence(sink: &mut dyn AdmitSink, admission: &StudyAdmission, nonce: &Nonce) {
    let Some(event) = study_evidence_event(admission, nonce) else {
        tracing::warn!(
            target: "aether_chassis_bloomery::intake",
            nonce = %nonce.0,
            "study artifact digest is not 32-byte hex; the attempt admits normally but its cost is unjournaled",
        );
        return;
    };
    match to_vec(&event) {
        Ok(bytes) => sink.admit(Admission { admit: Admit { event: bytes }, event }),
        Err(error) => tracing::warn!(
            target: "aether_chassis_bloomery::intake",
            nonce = %nonce.0,
            %error,
            "study evidence event could not encode; the attempt admits normally but its cost is unjournaled",
        ),
    }
}
