//! The re-adopted run: a lane child the coordinator inherited across a restart
//! (issue #4847) and re-attaches to by process identity when it can (issue #4999).
//!
//! [`reconcile`](crate::bloomery::executor::ReconcileLanes::reconcile) puts one
//! of these behind every live order whose scratch directories the previous
//! process left on disk. It stands in for the [`RunProcess`] a live dispatch
//! holds. When the previous process recorded a process identity and that
//! identity still names a live process, this run owns the child's process
//! group and can terminate it. Otherwise it is the unowned stand-in it always
//! was — it can observe the evidence file, and a cancel that asks it to kill
//! reports that it cannot.

use std::path::{Path, PathBuf};

use aether_bloomery::Nonce;

use super::error::LocalExecutorError;
#[cfg(unix)]
use super::identity::{self, ProcessIdentity};
use super::runner::{RunLifecycle, RunProcess};

/// A run re-adopted at boot: the coordinator knows where its evidence lands
/// and, when the previous process recorded one, the child's process identity.
pub struct OrphanedRun {
    nonce: Nonce,
    evidence_path: PathBuf,
    evidence_dir: PathBuf,
}

impl OrphanedRun {
    /// Re-adopt the run at `nonce`, reading its lifecycle from the
    /// `evidence.json` it would write into `evidence_dir` and its process
    /// identity from the sibling record, when one is present.
    #[must_use]
    pub fn new(nonce: Nonce, evidence_dir: &Path) -> Self {
        Self { nonce, evidence_path: evidence_dir.join("evidence.json"), evidence_dir: evidence_dir.to_path_buf() }
    }
}

impl RunProcess for OrphanedRun {
    /// The run's lifecycle as its evidence file and its recorded process
    /// identity report it — the two things still observable about a child this
    /// process never spawned.
    ///
    /// The evidence is asked first, and the order is the point: a written
    /// `evidence.json` means the run reached the end of its work, whatever the
    /// child did afterwards, so it reads as exited and the intake proceeds to
    /// consume it, recovering an attempt that finished while the coordinator was
    /// down. Asking about the process first would race a child that wrote its
    /// evidence and then exited into a host fault.
    ///
    /// With no evidence, the recorded identity decides. A pid that is no longer
    /// under `/proc` — or one whose start time or boot id no longer match, which
    /// is a recycled number and the same absence — is a child that died without
    /// writing, so it reads as exited and the executor host-faults and
    /// re-dispatches on the next poll. Before this, such a run held its slot and
    /// its member until the dispatch deadline, an hour for a construct lane
    /// (#5382).
    ///
    /// A run with no identity record on disk is the unowned stand-in this type
    /// always was: nothing here can tell a live child from a dead one, so it
    /// reads as running and the order rides on its dispatch deadline, which is
    /// the mechanism that exists to bound exactly that.
    ///
    /// The exit is a clean **failure**, not a signal and not a wait fault.
    /// This process never called `try_wait` on the child, so fabricating
    /// [`RunLifecycle::Signaled`] or [`RunLifecycle::ObservationFault`] would
    /// invent a process fact an orphan cannot observe (ADR-0195 §2). Fail-closed
    /// on the exit costs the evidence nothing: a bound authored body still
    /// drives the verdict, and a missing body is the host-fault path.
    fn poll(&mut self) -> RunLifecycle {
        if self.evidence_path.exists() {
            return RunLifecycle::Exited { success: false };
        }
        #[cfg(unix)]
        if let Some(recorded) = ProcessIdentity::read(&self.evidence_dir)
            && recorded.attach().is_none()
        {
            return RunLifecycle::Exited { success: false };
        }
        RunLifecycle::Running
    }

    /// Terminate the re-attached process group, or report that this process
    /// cannot.
    ///
    /// Re-attachment requires a recorded identity whose pid is live *and*
    /// whose start time and boot id still match. A missing, unreadable, or
    /// mismatched record is unowned: this method never signals an unverified
    /// pid, and it returns [`LocalExecutorError::Unterminated`] rather than
    /// `Ok(())` so a caller cannot treat the child as gone.
    ///
    /// A recorded identity whose pid is no longer live is the child already
    /// having exited — there is nothing to signal, and reporting success is
    /// honest. A live pid that does not match is a recycled number; signalling
    /// it would kill a stranger, so that stays unterminated.
    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        #[cfg(not(unix))]
        {
            let _ = &self.evidence_dir;
            return Err(LocalExecutorError::Unterminated(format!(
                "nonce `{}`: this platform cannot terminate a re-adopted lane child",
                self.nonce.0
            )));
        }
        #[cfg(unix)]
        {
            let Some(recorded) = ProcessIdentity::read(&self.evidence_dir) else {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    nonce = %self.nonce.0,
                    "cancelling a lane re-adopted from a previous coordinator process; no process identity was recorded, so its child cannot be terminated",
                );
                return Err(LocalExecutorError::Unterminated(format!(
                    "nonce `{}`: no process identity was recorded for this run",
                    self.nonce.0
                )));
            };
            if let Some(attached) = recorded.attach() {
                return attached.terminate_group();
            }
            if identity::pid_is_live(recorded.pid) {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    nonce = %self.nonce.0,
                    pid = recorded.pid,
                    "cancelling a lane re-adopted from a previous coordinator process; the live pid does not match the recorded start time or boot id, so it will not be signalled",
                );
                return Err(LocalExecutorError::Unterminated(format!(
                    "nonce `{}`: recorded process identity does not match the live pid {}",
                    self.nonce.0, recorded.pid
                )));
            }
            Ok(())
        }
    }
}
