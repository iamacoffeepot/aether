//! The pull-loop cycle: inspect each tracked run and, on completion, stream its
//! evidence through the broker to the sink.

use std::error::Error;
use std::fmt;

use aether_bloomery::{ExecutionStatus, Nonce, WorkHandle};

use super::admit::{Admission, AdmitDecision, IntakeError, admit_uploaded};
use super::claims::EvidenceClaims;
use crate::bloomery::executor::{ExecutorPortError, ExecutorShell};
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
            report.pending.push((handle.nonce.clone(), status));
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
