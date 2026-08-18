//! The transform spawn seam: the runner contract the backend is built over.
//!
//! Production shells out ([`ProcessTransformRunner`](super::ProcessTransformRunner));
//! tests substitute a stub that writes a canned output dir, so the backend's
//! registry / lifecycle / evidence logic is exercised without a real repo or
//! Claude credential.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use aether_bloomery::BackendObjectId;

use super::error::LocalExecutorError;

/// The fully-resolved spawn request a [`TransformRunner`] materializes: the lane
/// slot's checkout to bring to the subject, the evidence dir the run writes
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
    /// committed range an aggregate review judges. A Construct checkpoint
    /// marker never lands here — that provenance is [`seeded`](Self::seeded).
    pub diff_base_hex: Option<&'a str>,
    /// The construct checkpoint this dispatch resumes from (`--seeded`, #5052),
    /// rendered as the already-resolved checkout SHA. `None` on a clean
    /// Construct start and on every non-Construct lane. Independent of
    /// [`resume`](Self::resume): session reuse and tree seeding are separate.
    pub seeded: Option<&'a str>,
    /// Absolute path of the lane slot's canonical checkout — where this dispatch
    /// builds, and where every later dispatch in the same slot builds too.
    pub worktree_dir: &'a Path,
    /// Absolute path of the lane slot's own cargo target directory (#4912) — the
    /// `CARGO_TARGET_DIR` this dispatch and its verify gates build into, reused by
    /// every later dispatch in the same slot and shared with no other slot.
    ///
    /// Never inside [`worktree_dir`](Self::worktree_dir): the checkout is reset
    /// with `git clean --force --force -d -x` at the start of every dispatch, so a
    /// target directory in there would be deleted once per lap and the warm
    /// dependency tree lost with it.
    pub target_dir: &'a Path,
    /// How many build jobs this lane's cargo invocations may run at once
    /// (`CARGO_BUILD_JOBS`, #4912) — the cap that lets several lanes coexist in
    /// one host's memory. `0` leaves cargo's own default of one job per core.
    pub build_jobs: usize,
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
    /// The Claude session id a retry lap resumes (`--resume`), when the
    /// session pool leased one. `None` launches cold. The claude arm of
    /// `cargo xtask transform` threads this to `claude --resume`.
    pub resume: Option<&'a str>,
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
    /// The underlying kill/reap syscall failed, or the child could not be
    /// terminated at all — [`LocalExecutorError::Unterminated`]. The two are
    /// distinct from `Ok(())`: a caller that asked for termination and got
    /// success is entitled to believe the child is gone.
    fn kill(&mut self) -> Result<(), LocalExecutorError>;
}

/// A transform child's folded lifecycle.
///
/// A clean exit status, a terminating signal, and a `try_wait` fault stay
/// distinct: the first is whatever the child chose to report, the second and
/// third are host observations that rendered no judgment (ADR-0195 §2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunLifecycle {
    /// Still executing.
    Running,
    /// The child exited with a waitable status. `success` is whether that
    /// status was zero — a clean failure is still a clean exit, not a signal.
    Exited {
        /// Whether the child exited zero.
        success: bool,
    },
    /// The child was terminated by a signal. Unix only in production; tests
    /// pin the variant directly. Windows has no equivalent observation.
    Signaled {
        /// The signal that terminated the child (`SIGKILL` is 9).
        signal: i32,
    },
    /// `try_wait` failed; the host could not obtain a status at all.
    ObservationFault,
}

impl RunLifecycle {
    /// Fold a non-blocking `try_wait` into the typed lifecycle.
    #[must_use]
    pub fn from_try_wait(result: io::Result<Option<ExitStatus>>) -> Self {
        result.map_or(Self::ObservationFault, |status| status.map_or(Self::Running, Self::from_exit_status))
    }

    /// Classify a reaped [`ExitStatus`] without collapsing a signal into a
    /// boolean.
    #[must_use]
    pub fn from_exit_status(status: ExitStatus) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Self::Signaled { signal };
            }
        }
        Self::Exited { success: status.success() }
    }

    /// Whether this lifecycle will never change again.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    /// Whether the child exited zero. A signal or a wait fault is not a
    /// success, even though both are terminal.
    #[must_use]
    pub const fn clean_success(self) -> bool {
        matches!(self, Self::Exited { success: true })
    }
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

    /// Tear down an abandoned checkout: remove the directory and the `git
    /// worktree` registration together.
    ///
    /// Boot reconciliation's reclaim, and only that. A run's own terminal path
    /// releases the lane *slot* rather than the checkout in it — the path is
    /// canonical and the next dispatch to hold the slot resets the tree — so what
    /// reaches this seam is a checkout no order is waiting on and no slot claims:
    /// a nonce-keyed one left by a coordinator from before that layout.
    ///
    /// # Errors
    /// The worktree teardown (the `git worktree remove` shell-out) failed.
    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError>;

    /// Every scratch checkout the backing repository currently has registered.
    ///
    /// The boot reconciliation's discriminator (issue #4847): the scratch root is
    /// a configured path, so a directory listing of it proves nothing about who
    /// made an entry, while a registration under `base_dir` can only have come
    /// from this backend's own [`start`](Self::start). Which of those are the
    /// lane slots' — reused rather than reclaimable — and which are abandoned is
    /// the caller's to tell apart, as are paths outside the root: this seam
    /// reports what the repository knows, not what the backend owns.
    ///
    /// # Errors
    /// The registration read (the `git worktree list` shell-out) failed.
    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError>;

    /// Capture a passed model-lane run's working-tree changes as the candidate
    /// (ADR-0152): stage and commit everything in the run worktree under the
    /// bloomery's own identity and return the produced commit + tree object ids,
    /// or `Ok(None)` when the worktree is clean (nothing to capture). Runs on
    /// the run's terminal path, while the slot's checkout still holds the work —
    /// which is until the next dispatch takes that slot and resets it — and only
    /// in the host's trust domain: the child never stages, commits, or holds
    /// credentials.
    ///
    /// `message` is the commit message the run's own lane wrote, when it wrote
    /// one: the model that made the change names it, and the capture commits
    /// under that message's first line instead of a flat literal. `None` falls
    /// back to the literal, so a lane that produced no message still captures.
    ///
    /// # Errors
    /// A git shell-out (status / add / commit / rev-parse) failed.
    fn capture(
        &self,
        worktree_dir: &Path,
        message: Option<&str>,
    ) -> Result<Option<CapturedObjects>, LocalExecutorError>;

    /// Direct commit parents of a checkout, in git parent order.
    ///
    /// The backend consults this after an exact builder-slot miss so a
    /// synthetic fold commit can still prefer the slot that captured its
    /// dependency parent. A non-commit checkout reports no parents; an
    /// unreadable one is `Err`. Both degrade to no preference rather than
    /// refusing the order.
    ///
    /// The default reports none — a stub that does not inspect Git. Production
    /// reads the source repository; the fixed runner supplies deterministic
    /// fixtures.
    ///
    /// # Errors
    /// The object could not be inspected (a git shell-out failed).
    fn checkout_parents(&self, _checkout_hex: &str) -> Result<Vec<String>, LocalExecutorError> {
        Ok(Vec::new())
    }
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
    /// The lap's own unified diff — the capture commit against the checkout it
    /// was built on (#4959), which for a repair lap is exactly what that lap
    /// changed. Read by the repair-lap triage, and by nothing else: a candidate
    /// is identified by its tree, never by this text.
    ///
    /// `None` when the runner could not produce one. The triage passes a lap it
    /// cannot inspect, so a shortfall here costs a check and never a candidate.
    pub diff: Option<String>,
}
