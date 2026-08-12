//! The pull-loop cycle: inspect each tracked run and, on completion, stream its
//! evidence through the broker to the sink.

use std::error::Error;
use std::fmt;

use aether_bloomery::{ExecutionStatus, Nonce, WorkHandle};

use super::admit::{Admission, AdmitDecision, IntakeError, UploadedEvidence, admit_uploaded};
use super::claims::EvidenceClaims;
use crate::artifacts::ArtifactsCapabilityState;
use crate::bloomery::executor::{ExecutorPortError, ExecutorShell};
use crate::bloomery::study::{StudyAdmitDecision, UploadedStudyRecord, admit_study};
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
    /// second `inspect` call.
    pub pending: Vec<(Nonce, ExecutionStatus)>,
}

/// A fault during an intake cycle.
#[derive(Debug)]
pub enum CycleError {
    /// Inspecting a run faulted.
    Inspect(ExecutorPortError),
    /// Streaming a run's evidence faulted.
    Stream(ExecutorPortError),
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
/// Runs *before* the caller's deadline sweep, and the order is load-bearing
/// (ADR-0177): evidence that arrived at or just before the deadline boundary is
/// admitted normally and consumes its order, so the sweep that follows never
/// sees it and a lane that finished in time is never cancelled for being late to
/// be observed. Only an order still pending after this cycle can expire.
///
/// # Errors
/// [`CycleError`] if inspecting a run, streaming its evidence, or the broker
/// faulted; a clean broker refusal is counted, not an error.
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
    for handle in handles {
        let status = shell.inspect(handle).map_err(CycleError::Inspect)?;
        if !matches!(status, ExecutionStatus::Completed { .. }) {
            report.pending.push((handle.nonce.clone(), status));
            continue;
        }
        report.completed += 1;
        for reference in shell.stream_evidence(handle).map_err(CycleError::Stream)? {
            let Some(upload) = claims.claim_for(&reference) else {
                continue;
            };
            // Study first, and deliberately: `admit_study` matches the order
            // *without* consuming it, while `admit_uploaded` below consumes.
            // Reversed, every study upload would look up an already-spent nonce
            // and refuse as `UnknownNonce` — the lane would be wired and still
            // record nothing, which is the failure this issue exists to end.
            report.studied += u32::from(record_cost(store, artifacts.as_deref_mut(), &upload));
            match admit_uploaded(store, &upload).map_err(CycleError::Intake)? {
                AdmitDecision::Admitted(admission) => {
                    report.admitted += 1;
                    sink.admit(*admission);
                }
                AdmitDecision::Refused(refusal) => {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::intake",
                        nonce = %upload.nonce.0,
                        ?refusal,
                        "attempt evidence refused",
                    );
                    report.refused += 1;
                }
            }
        }
    }
    Ok(report)
}

/// Record what one attempt cost, returning whether a study row was written
/// (#4679).
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
) -> bool {
    let (Some(cost), Some(artifacts)) = (upload.cost, artifacts) else {
        return false;
    };
    let record = UploadedStudyRecord { nonce: upload.nonce.clone(), subject: upload.subject, cost };

    match admit_study(store, artifacts, &record) {
        Ok(StudyAdmitDecision::Admitted(_)) => true,
        Ok(StudyAdmitDecision::Refused(refusal)) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::intake",
                nonce = %upload.nonce.0,
                ?refusal,
                "study record refused; the attempt admits normally but its cost is unrecorded",
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::intake",
                nonce = %upload.nonce.0,
                %error,
                "study record could not be stored; the attempt admits normally but its cost is unrecorded",
            );
            false
        }
    }
}
