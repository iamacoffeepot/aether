//! The transform spawn seam: the runner contract the backend is built over.
//!
//! Production shells out ([`ProcessTransformRunner`](super::ProcessTransformRunner));
//! tests substitute a stub that writes a canned output dir, so the backend's
//! registry / lifecycle / evidence logic is exercised without a real repo or
//! Claude credential.

use std::path::Path;

use aether_bloomery::BackendObjectId;

use super::error::LocalExecutorError;

/// The fully-resolved spawn request a [`TransformRunner`] materializes: the
/// scratch worktree to check the subject into, the evidence dir the run writes
/// `evidence.json` to, and the `cargo xtask transform` argv shape.
pub struct RunSpec<'a> {
    /// The typed transform command id (`verify.*` or `construct.implement`).
    pub command: &'a str,
    /// The order's checkout target rendered to hex — the exact git commit the
    /// worktree is materialized at (the sealed source, ADR-0149 §Execution).
    pub checkout_hex: &'a str,
    /// The order's diff base rendered to hex (`--diff-base`, #4723) — the commit
    /// the candidate is judged *against*, when the order names one. `None` is the
    /// working-tree contract every member lane runs under; `Some` is the
    /// committed range an aggregate review judges.
    pub diff_base_hex: Option<&'a str>,
    /// Absolute path the scratch worktree is created at (keyed by nonce).
    pub worktree_dir: &'a Path,
    /// Absolute path the run writes its `evidence.json` to (`--out`).
    pub evidence_dir: &'a Path,
    /// The idempotency nonce stamped into the evidence (`--nonce`).
    pub nonce: &'a str,
    /// The resolved harness a model lane forks (`--harness`) — which agent CLI
    /// executes the stage. `None` for a mechanical verify lane, which runs a
    /// compiler and ignores it, exactly as it ignores `--model`.
    pub harness: Option<&'a str>,
    /// The resolved model the `construct.implement` lane runs under (`--model`);
    /// `None` for a mechanical verify lane, which ignores it.
    pub model: Option<&'a str>,
    /// The resolved reasoning-effort tier (`--effort`); `None` for a verify lane.
    pub effort: Option<&'a str>,
    /// The advisory work-order description the construct lane names in its
    /// prompt's `## Task` section (`--task`, #3595); `None` when the coordinator
    /// persisted none (a subject-only prompt) or for a verify lane, which ignores
    /// it exactly as it ignores `--model`.
    pub task: Option<&'a str>,
}

/// A running (or finished) transform child — the lifecycle the backend maps onto
/// [`ExecutionStatus`](aether_bloomery::ExecutionStatus). `Send` so a run can live
/// in the backend's registry behind an `Arc<dyn ExecutorBackend + Send + Sync>`.
pub trait RunProcess: Send {
    /// The child's current lifecycle, without blocking.
    fn poll(&mut self) -> RunLifecycle;
    /// Kill the child (and reap it).
    ///
    /// # Errors
    /// The underlying kill/reap syscall failed.
    fn kill(&mut self) -> Result<(), LocalExecutorError>;
}

/// A transform child's folded lifecycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunLifecycle {
    /// Still executing.
    Running,
    /// Exited; `success` mirrors the child's exit status.
    Exited {
        /// Whether the child exited zero.
        success: bool,
    },
}

/// The git-checkout + `cargo xtask transform` spawn seam. Production shells out
/// ([`ProcessTransformRunner`](super::ProcessTransformRunner)); tests substitute a
/// stub that writes a canned output dir, so the backend's registry / lifecycle /
/// evidence logic is exercised without a real repo or Claude credential.
pub trait TransformRunner: Send + Sync {
    /// Materialize the checkout and spawn the transform, returning a handle to
    /// the running child.
    ///
    /// # Errors
    /// The worktree checkout or the child spawn failed.
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError>;

    /// Release a run's scratch worktree once it reaches a terminal state — a
    /// cancel, or a consumed evidence read. Best-effort teardown that the backend
    /// logs on failure rather than propagating, so a leaked-worktree cleanup miss
    /// never fails the cancel the kill already completed or the evidence stream.
    ///
    /// # Errors
    /// The worktree teardown (the `git worktree remove` shell-out) failed.
    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError>;

    /// Capture a passed model-lane run's working-tree changes as the candidate
    /// (ADR-0152): stage and commit everything in the run worktree under the
    /// bloomery's own identity and return the produced commit + tree object ids,
    /// or `Ok(None)` when the worktree is clean (nothing to capture). Runs on
    /// the run's terminal path, before [`release`](Self::release) discards the
    /// worktree — and only in the host's trust domain: the child never stages,
    /// commits, or holds credentials.
    ///
    /// # Errors
    /// A git shell-out (status / add / commit / rev-parse) failed.
    fn capture(&self, worktree_dir: &Path) -> Result<Option<CapturedObjects>, LocalExecutorError>;
}

/// What [`TransformRunner::capture`] produced: the capture commit wrapping the
/// run's tree, and that tree itself — the backend-object side of ADR-0152's
/// two-digest [`CandidateRef`](aether_bloomery::CandidateRef) (`checkout` ↔
/// commit, `tree` ↔ tree).
///
/// Both identifiers stay opaque here. Which byte shapes are well-formed object
/// ids is the Git adapter's question, not the host capture path's; this seam only
/// carries the bytes the runner produced through to the correspondence.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CapturedObjects {
    /// The capture commit's object id (parent = the run's checkout).
    pub commit: BackendObjectId,
    /// The captured tree's object id — the candidate's content identity.
    pub tree: BackendObjectId,
}
