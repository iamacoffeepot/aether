//! The re-adopted run: a lane child the coordinator inherited across a restart
//! but does not own (issue #4847).
//!
//! [`reconcile`](crate::bloomery::executor::ReconcileLanes::reconcile) puts one
//! of these behind every live order whose scratch directories the previous
//! process left on disk. It stands in for the [`RunProcess`] a live dispatch
//! holds, and the difference between the two is the whole point: an owned run
//! wraps a [`std::process::Child`] it can wait on and kill, while this one has
//! only the run's output directory to look at.

use std::path::{Path, PathBuf};

use aether_bloomery::Nonce;

use super::error::LocalExecutorError;
use super::runner::{RunLifecycle, RunProcess};

/// A run re-adopted at boot: the coordinator knows where its evidence lands and
/// nothing else about it.
pub struct OrphanedRun {
    nonce: Nonce,
    evidence_path: PathBuf,
}

impl OrphanedRun {
    /// Re-adopt the run at `nonce`, reading its lifecycle from the
    /// `evidence.json` it would write into `evidence_dir`.
    #[must_use]
    pub fn new(nonce: Nonce, evidence_dir: &Path) -> Self {
        Self { nonce, evidence_path: evidence_dir.join("evidence.json") }
    }
}

impl RunProcess for OrphanedRun {
    /// The run's lifecycle as its evidence file reports it — the only thing
    /// still observable about a child this process never spawned.
    ///
    /// A written `evidence.json` means the run reached the end of its work, so
    /// it reads as exited and the intake proceeds to consume the evidence,
    /// recovering an attempt that finished while the coordinator was down. An
    /// absent one means the run either is still going or died without writing,
    /// and the two are indistinguishable from here — so it reads as running and
    /// the order rides on its dispatch deadline, which is the mechanism that
    /// exists to bound exactly this.
    ///
    /// The exit is reported as a **failure** because no zero exit was ever
    /// observed, and fail-closed is what the rest of this backend does with an
    /// unobservable run. It costs the evidence nothing: the construct gate
    /// ignores the exit entirely and the verify gate rides its stamped `status`,
    /// so this decides a verdict only for a body that stamps neither — the shape
    /// that already fails closed.
    fn poll(&mut self) -> RunLifecycle {
        if self.evidence_path.exists() {
            RunLifecycle::Exited { success: false }
        } else {
            RunLifecycle::Running
        }
    }

    /// Report the child as unreclaimable and let the cancel proceed.
    ///
    /// The coordinator cannot kill what it does not hold: there is no handle
    /// across the restart, no recorded pid, and no portable syscall on this
    /// crate's path to use one with. Failing the cancel instead would strand the
    /// order — its expiry would refuse to consume it and the member would never
    /// retry — while still not killing anything, so the cancel proceeds and what
    /// it reclaims is the lane slot the run held.
    ///
    /// Not the checkout in that slot. Removing it would be the closest thing to
    /// termination available here — a lane whose working directory is gone fails
    /// its next write — but the path is the slot's rather than the run's, so the
    /// tree that would be pulled belongs to whichever dispatch holds the slot by
    /// then. Freeing the slot is the honest reclaim; the surviving child is left
    /// to whatever it does next.
    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            nonce = %self.nonce.0,
            "cancelling a lane re-adopted from a previous coordinator process; its child cannot be terminated by this process and may still be running",
        );
        Ok(())
    }
}
