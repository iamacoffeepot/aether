//! The local-process executor-port backend (ADR-0149 §The boundary, ADR-0150
//! credentials-never-leave-the-machine — issue #3586).
//!
//! The Actions backend ([`ActionsExecutor`](aether_bloomery_github::ActionsExecutor))
//! dispatches a work order at a wrapper workflow on a shared runner. That is
//! correct for the zero-secret mechanical verify lanes, but the model-driven
//! `construct.implement` lane runs headless Claude, whose non-delegable
//! subscription credential ADR-0150 forbids from ever living in a GitHub secret
//! store. This backend reproduces on the operator's own machine exactly what the
//! wrapper does on a runner — materialize the order's checkout into a scratch
//! worktree and spawn the same `cargo xtask transform <command>` entrypoint under
//! ambient local `claude` auth — so the model lane needs no fork and no secret.
//!
//! # The nonce is the handle, the output dir is the evidence
//!
//! Like the Actions backend, `submit` returns the order's [`Nonce`] as the
//! handle and the other three messages resolve the run from it — here through an
//! in-process registry the backend owns rather than a `workflow_dispatch`
//! resolution. Because the backend owns the run's output directory directly, it
//! synthesizes the nonce-tagged [`EvidenceRef`] name from the run's
//! `evidence.json` itself (`attempt.<verdict>.<subject_hex>.<detail_hex>.<nonce>`,
//! the [`attempt_artifact_name`] contract the intake path's
//! [`NameEvidenceClaims`](super::intake::NameEvidenceClaims) decodes) — no
//! artifact-upload naming step to depend on.
//!
//! # The spawn seam
//!
//! The git-checkout + `cargo xtask` shell-out is behind the [`TransformRunner`]
//! trait so the backend's registry / lifecycle / evidence-synthesis logic is
//! unit-testable against a stub that writes a canned output dir, without a real
//! git repo or a Claude credential. Production mounts [`ProcessTransformRunner`].

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf, absolute};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::{fs, io};

use aether_bloomery::digest::ContentAddressed;
use aether_bloomery::{
    CandidateRef, Conclusion, Digest, EvidenceRef, ExecutionStatus, ExecutorBackend, Nonce, WorkHandle, WorkOrder,
    digest_of,
};
use aether_bloomery_github::{CorrespondenceError, GitObjectId, SharedCorrespondence, StageVerdict};
use serde::Serialize;

use super::CONSTRUCT_IMPLEMENT_COMMAND;
use super::intake::attempt_artifact_name;

// The typed id of the model-driven review lane. It rides the same model-lane
// plumbing as the construct command — subject/model/effort/task argv — but not
// the construct-specific evidence gate: its verdict comes from the `status`
// its own evidence stamps. The catalog's exported constant (#3668).
use aether_bloomery::REVIEW_CRITIC_COMMAND;

/// A local-process executor-port fault. Its own type because the port needs an
/// arm the value vocabulary does not carry — a message asked to act on a run
/// that does not resolve for its nonce — alongside the worktree / spawn / io /
/// evidence faults the local lane produces.
#[derive(Debug)]
pub enum LocalExecutorError {
    /// Materializing the order's checkout into the scratch worktree failed (the
    /// `git worktree add` shell-out, carrying its stderr tail).
    Worktree(String),
    /// Spawning the `cargo xtask transform` child failed.
    Spawn(io::Error),
    /// A filesystem operation (create the run dir, kill/reap the child) failed.
    Io(io::Error),
    /// The run's `evidence.json` could not be read (the run wrote none, or the
    /// read faulted).
    Evidence(String),
    /// A `cancel` / `stream_evidence` resolved no tracked run for the nonce — the
    /// order was never submitted to this backend, or was already consumed.
    /// (`inspect` reports the same condition as the clean [`ExecutionStatus::Unknown`].)
    NoRunForNonce(Nonce),
    /// The order's checkout digest resolved no real git object through the
    /// correspondence store (ADR-0150) — the sealed source was never materialized
    /// or its correspondence never seeded, so the backend refuses cleanly rather
    /// than `git worktree add`-ing a target git cannot resolve.
    UnresolvedCheckout(Nonce),
    /// The correspondence store itself faulted while resolving the checkout.
    Correspondence(CorrespondenceError),
}

impl fmt::Display for LocalExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Worktree(detail) => write!(f, "local executor backend: worktree checkout failed: {detail}"),
            Self::Spawn(error) => write!(f, "local executor backend: spawn transform failed: {error}"),
            Self::Io(error) => write!(f, "local executor backend: {error}"),
            Self::Evidence(detail) => write!(f, "local executor backend: evidence read failed: {detail}"),
            Self::NoRunForNonce(nonce) => {
                write!(f, "local executor backend: no run resolves for nonce `{}`", nonce.0)
            }
            Self::UnresolvedCheckout(nonce) => {
                write!(
                    f,
                    "local executor backend: no git-object correspondence for the checkout of nonce `{}`",
                    nonce.0
                )
            }
            Self::Correspondence(error) => write!(f, "local executor backend: {error}"),
        }
    }
}

impl Error for LocalExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Io(error) => Some(error),
            Self::Correspondence(error) => Some(error),
            Self::Worktree(_) | Self::Evidence(_) | Self::NoRunForNonce(_) | Self::UnresolvedCheckout(_) => None,
        }
    }
}

impl From<CorrespondenceError> for LocalExecutorError {
    fn from(error: CorrespondenceError) -> Self {
        Self::Correspondence(error)
    }
}

/// The fully-resolved spawn request a [`TransformRunner`] materializes: the
/// scratch worktree to check the subject into, the evidence dir the run writes
/// `evidence.json` to, and the `cargo xtask transform` argv shape.
pub struct RunSpec<'a> {
    /// The typed transform command id (`verify.*` or `construct.implement`).
    pub command: &'a str,
    /// The order's checkout target rendered to hex — the exact git commit the
    /// worktree is materialized at (the sealed source, ADR-0149 §Execution).
    pub checkout_hex: &'a str,
    /// Absolute path the scratch worktree is created at (keyed by nonce).
    pub worktree_dir: &'a Path,
    /// Absolute path the run writes its `evidence.json` to (`--out`).
    pub evidence_dir: &'a Path,
    /// The idempotency nonce stamped into the evidence (`--nonce`).
    pub nonce: &'a str,
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
/// [`ExecutionStatus`]. `Send` so a run can live in the backend's registry behind
/// an `Arc<dyn ExecutorBackend + Send + Sync>`.
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
/// ([`ProcessTransformRunner`]); tests substitute a stub that writes a canned
/// output dir, so the backend's registry / lifecycle / evidence logic is
/// exercised without a real repo or Claude credential.
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
/// run's tree, and that tree itself — the git side of ADR-0152's two-digest
/// [`CandidateRef`] (`checkout` ↔ commit, `tree` ↔ tree).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CapturedObjects {
    /// The capture commit's object id (parent = the run's checkout).
    pub commit: GitObjectId,
    /// The captured tree's object id — the candidate's content identity.
    pub tree: GitObjectId,
}

/// One tracked run: the spawned child, its scratch worktree, where its evidence
/// lands, and the digest the returning evidence must bind to.
struct Run {
    process: Box<dyn RunProcess>,
    // The scratch worktree `start` materialized the checkout into, released on the
    // run's terminal path (cancel, or evidence consumed) so a long-lived backend
    // does not leak one `git worktree` per order.
    worktree_dir: PathBuf,
    evidence_dir: PathBuf,
    // The digest the intake broker binds the evidence to — the order's subject
    // input (`transformation.inputs[0]`, what `drain_and_dispatch` records as the
    // displayed digest), NOT the checkout target. The two are distinct axes: the
    // checkout is the tree the work runs on, the subject is what the evidence is
    // about. Binding to the checkout would refuse at intake as a digest mismatch.
    subject: Digest,
    // Whether this run is the model-driven construct lane, decided at submit from
    // the order's command. The completion gate is lane-specific: a construct run's
    // verdict demands a substantive conclusion (#3596), a verify run's rides its
    // stamped `status`, so the gate must know which lane produced the evidence —
    // and must know it even when the evidence bytes do not decode (fail-closed).
    is_construct: bool,
}

/// The local-process executor backend: an in-process registry of tracked runs
/// keyed by nonce, over a [`TransformRunner`] spawn seam.
pub struct LocalExecutor {
    runner: Arc<dyn TransformRunner>,
    correspondence: SharedCorrespondence,
    base_dir: PathBuf,
    construct_model: Option<String>,
    construct_effort: Option<String>,
    runs: Mutex<HashMap<String, Run>>,
}

impl LocalExecutor {
    /// Build a backend over an explicit spawn seam — the seam tests drive with a
    /// stub runner, and [`from_config`](Self::from_config) drives with the
    /// production [`ProcessTransformRunner`]. `base_dir` is the scratch-worktree
    /// root; each run gets `base_dir/<nonce>` (worktree) and
    /// `base_dir/<nonce>-evidence` (output).
    #[must_use]
    pub fn new(
        runner: Arc<dyn TransformRunner>,
        correspondence: SharedCorrespondence,
        base_dir: impl Into<PathBuf>,
        construct_model: Option<String>,
        construct_effort: Option<String>,
    ) -> Self {
        Self {
            runner,
            correspondence,
            base_dir: base_dir.into(),
            construct_model,
            construct_effort,
            runs: Mutex::new(HashMap::new()),
        }
    }

    /// Build the production backend from resolved config: the real git + cargo
    /// [`ProcessTransformRunner`], the shared `correspondence` the checkout
    /// resolves through, the config'd scratch-worktree base dir, and the config'd
    /// construct model/effort (empty strings resolve to `None`).
    #[must_use]
    pub fn from_config(config: &super::mirror::GithubMirrorConfig, correspondence: SharedCorrespondence) -> Self {
        Self::new(
            Arc::new(ProcessTransformRunner),
            correspondence,
            config.local_worktree_base.clone(),
            non_empty(&config.local_construct_model),
            non_empty(&config.local_construct_effort),
        )
    }

    // Lock the registry, recovering the guard on a poisoned mutex rather than
    // panicking — a backend is long-lived behind an Arc and a poisoned lock
    // should degrade to best-effort, not take the whole coordinator down.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Run>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // Release a terminal run's scratch worktree off the registry lock (the teardown
    // is a blocking git shell-out), folding a failure into a warn rather than the
    // terminal op's result — the child is already dead / the evidence already read,
    // so a cleanup miss must not fail the cancel or the evidence stream.
    fn release_worktree(&self, worktree_dir: &Path) {
        if let Err(error) = self.runner.release(worktree_dir) {
            tracing::warn!(
                worktree = %worktree_dir.display(),
                %error,
                "local executor backend: scratch worktree release failed",
            );
        }
    }

    // Capture a passed construct-lane run's candidate (ADR-0152): commit the run
    // worktree's changes through the runner seam, then record both produced git
    // objects as correspondence rows under their content-derived digests. Every
    // shortfall — a shell fault, a clean worktree (contradicting the passed
    // substantive-conclusion gate), a store write fault — folds to `None` with a
    // warn; the caller downgrades the verdict, so a lost capture reads as a
    // failed attempt, never a pass whose work silently evaporated.
    fn capture_candidate(&self, worktree_dir: &Path, nonce: &Nonce) -> Option<CandidateRef> {
        let captured = match self.runner.capture(worktree_dir) {
            Ok(Some(captured)) => captured,
            Ok(None) => {
                tracing::warn!(
                    nonce = %nonce.0,
                    "local executor backend: passed run left a clean worktree — nothing to capture, failing closed",
                );
                return None;
            }
            Err(error) => {
                tracing::warn!(nonce = %nonce.0, %error, "local executor backend: candidate capture failed");
                return None;
            }
        };
        let candidate = CandidateRef {
            tree: candidate_tree_digest(&captured.tree),
            checkout: capture_commit_digest(&captured.commit),
        };
        match self
            .correspondence
            .record(&candidate.tree, &captured.tree)
            .and_then(|()| self.correspondence.record(&candidate.checkout, &captured.commit))
        {
            Ok(()) => Some(candidate),
            Err(error) => {
                tracing::warn!(nonce = %nonce.0, %error, "local executor backend: candidate correspondence write failed");
                None
            }
        }
    }
}

/// The content-derived digest of a captured candidate tree: a domain-tagged
/// address over the git tree object id, so the digest changes exactly when the
/// captured content does — ADR-0152's supersession property falls out of the
/// identity choice.
#[derive(Serialize)]
struct CandidateTreeAddress<'a> {
    object: &'a [u8],
}

impl ContentAddressed for CandidateTreeAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.candidate.tree";
}

fn candidate_tree_digest(tree: &GitObjectId) -> Digest {
    digest_of(&CandidateTreeAddress { object: tree.bytes() })
}

/// The content-derived digest of a capture commit — the [`CandidateRef::checkout`]
/// axis, distinct from the tree's by domain tag so the two never collide even
/// over equal object bytes.
#[derive(Serialize)]
struct CaptureCommitAddress<'a> {
    object: &'a [u8],
}

impl ContentAddressed for CaptureCommitAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.candidate.checkout";
}

fn capture_commit_digest(commit: &GitObjectId) -> Digest {
    digest_of(&CaptureCommitAddress { object: commit.bytes() })
}

impl ExecutorBackend for LocalExecutor {
    type Error = LocalExecutorError;

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        let nonce = order.nonce.0.clone();
        // Resolve both run paths absolute against the coordinator's own cwd before
        // the spawn. The child runs with `current_dir(worktree_dir)`, so a relative
        // `--out` (the config default `local_worktree_base` ships relative) would
        // resolve against the *child's* cwd — the scratch worktree — while
        // `stream_evidence` reads `evidence_dir` against the *coordinator's* cwd; the
        // two diverge and the intake polls a path the run never wrote, forever.
        // `std::path::absolute` is a lexical cwd-join that does not require the path
        // to exist (unlike `canonicalize`).
        let worktree_dir = absolute(self.base_dir.join(&nonce)).map_err(LocalExecutorError::Io)?;
        let evidence_dir = absolute(self.base_dir.join(format!("{nonce}-evidence"))).map_err(LocalExecutorError::Io)?;
        // Resolve the sealed checkout digest to its real git object sha through the
        // correspondence store (ADR-0150) — the `git worktree add` target — rather
        // than hex-punning the digest into a name git cannot resolve.
        let checkout_hex = self
            .correspondence
            .resolve_git(&order.transformation.checkout)?
            .ok_or_else(|| LocalExecutorError::UnresolvedCheckout(order.nonce.clone()))?
            .to_hex();
        // The subject the returning evidence binds to is the order's subject input
        // (the scope-revision digest the broker displayed), falling back to the
        // checkout only for a malformed order that carries no input.
        let subject = order.transformation.inputs.first().copied().unwrap_or(order.transformation.checkout);
        // Model/effort/task ride the model-driven lanes (construct and the
        // review critic), mirroring `transform-model.yml`'s argv; a verify lane
        // ignores them. `is_construct` stays narrower — it selects the
        // construct-specific evidence gate (substantive-conclusion, #3596),
        // which the review lane's `status`-stamped evidence must not ride.
        let is_construct = order.transformation.command == CONSTRUCT_IMPLEMENT_COMMAND;
        let is_model_lane = is_construct || order.transformation.command == REVIEW_CRITIC_COMMAND;
        let spec = RunSpec {
            command: &order.transformation.command,
            checkout_hex: &checkout_hex,
            worktree_dir: &worktree_dir,
            evidence_dir: &evidence_dir,
            nonce: &nonce,
            model: is_model_lane.then_some(self.construct_model.as_deref()).flatten(),
            effort: is_model_lane.then_some(self.construct_effort.as_deref()).flatten(),
            // The work-order description rides the order's transformation (#3595),
            // populated at dispatch from durable state; the model lanes name it
            // (the critic judges the candidate against it), mirroring the
            // model/effort gate.
            task: is_model_lane.then_some(order.transformation.description.as_deref()).flatten(),
        };
        let process = self.runner.start(&spec)?;
        self.lock().insert(nonce, Run { process, worktree_dir, evidence_dir, subject, is_construct });
        Ok(WorkHandle::new(order.nonce.clone()))
    }

    // `run` is a `&mut` reborrow from the registry guard (poll mutates the child),
    // so the guard must outlive it; the lint's "merge into a single expression" fix
    // would drop the guard before the reborrow, so it is suppressed here.
    #[allow(clippy::significant_drop_tightening, reason = "run is a &mut reborrow; the guard must outlive it")]
    fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        // Read the lifecycle out of the guarded region and drop the lock before
        // returning — the guard need only be held for the poll.
        let lifecycle = {
            let mut runs = self.lock();
            let Some(run) = runs.get_mut(&handle.nonce.0) else {
                // Not tracked here is the clean Unknown, never an error — the same
                // "dispatch async, not visible yet" state the Actions backend reports.
                // A cancelled run has been evicted, so it also reports Unknown here
                // rather than reading the killed child's exit as a plain completion.
                return Ok(ExecutionStatus::Unknown);
            };
            run.process.poll()
        };
        Ok(match lifecycle {
            RunLifecycle::Running => ExecutionStatus::Running,
            RunLifecycle::Exited { success: true } => ExecutionStatus::Completed { conclusion: Conclusion::Success },
            RunLifecycle::Exited { success: false } => ExecutionStatus::Completed { conclusion: Conclusion::Failure },
        })
    }

    fn cancel(&self, handle: &WorkHandle) -> Result<(), Self::Error> {
        // Kill and evict under the lock, then pull the run's worktree out so the
        // teardown runs off the lock. A failed kill returns early, leaving both the
        // entry and the worktree in place.
        let worktree_dir = {
            let mut runs = self.lock();
            let Some(run) = runs.get_mut(&handle.nonce.0) else {
                return Err(LocalExecutorError::NoRunForNonce(handle.nonce.clone()));
            };
            run.process.kill()?;
            let worktree_dir = run.worktree_dir.clone();
            // A cancel is terminal — evict the killed run so the registry tracks only
            // in-flight orders rather than parking `cancelled` entries forever.
            runs.remove(&handle.nonce.0);
            worktree_dir
        };
        self.release_worktree(&worktree_dir);
        Ok(())
    }

    #[allow(clippy::significant_drop_tightening, reason = "run is a &mut reborrow; the guard must outlive it")]
    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        // Pull the run's on-disk location, binding digest, and terminal exit out of
        // the guarded region, then drop the lock — the evidence read is blocking IO
        // and must not hold the registry mutex.
        let (evidence_dir, subject, lifecycle, is_construct, worktree_dir) = {
            let mut runs = self.lock();
            let Some(run) = runs.get_mut(&handle.nonce.0) else {
                return Err(LocalExecutorError::NoRunForNonce(handle.nonce.clone()));
            };
            (run.evidence_dir.clone(), run.subject, run.process.poll(), run.is_construct, run.worktree_dir.clone())
        };
        let exited_success = matches!(lifecycle, RunLifecycle::Exited { success: true });
        let evidence_path = evidence_dir.join("evidence.json");
        let bytes = match fs::read(&evidence_path) {
            Ok(bytes) => bytes,
            // The run's own lifecycle is the terminal-vs-transient discriminator. An
            // Exited run that has left no readable evidence never will — re-driving
            // the read against it loops forever (the live 2026-07-18 bug), so this is
            // terminal: evict, release the worktree, and synthesize a fail-closed
            // VerificationFailed attempt that feeds the retry/wedge machinery rather
            // than an error the intake re-drives. A Running run's missing file is
            // transient — keep the entry and worktree for the next cycle's retry.
            Err(read_error) => {
                if matches!(lifecycle, RunLifecycle::Exited { .. }) {
                    // Log the real IO fault before folding it into a fail-closed
                    // verdict — a permission/disk fault reads identically to a
                    // genuinely-absent evidence file once synthesized, so the fault
                    // must stay visible in the operator's logs (the same best-effort
                    // warn convention `release_worktree` uses).
                    tracing::warn!(
                        nonce = %handle.nonce.0,
                        evidence = %evidence_path.display(),
                        error = %read_error,
                        "local executor backend: exited run left no readable evidence — failing closed",
                    );
                    self.lock().remove(&handle.nonce.0);
                    self.release_worktree(&worktree_dir);
                    return Ok(vec![EvidenceRef {
                        name: attempt_artifact_name(
                            &handle.nonce,
                            &subject,
                            StageVerdict::VerificationFailed,
                            &Digest::of_wire_bytes(&[]),
                        ),
                        nonce: handle.nonce.clone(),
                        artifact_id: 0,
                        size_bytes: 0,
                        candidate: None,
                        findings: None,
                    }]);
                }
                return Err(LocalExecutorError::Evidence(format!("{}: {read_error}", evidence_path.display())));
            }
        };
        // Verdict from the run's own evidence, lane-specific. The construct lane's
        // gate demands a substantive conclusion (#3596) — a terminal `result` with
        // `is_error == false` AND a produced candidate — and is fail-closed on any
        // shortfall (dead run, errored run, empty candidate, unparseable evidence),
        // so it never falls back to the child's terminal exit (an empty run exits
        // zero). The verify lane stamps a `status` ("pass"/"fail"); the raw
        // `exited_success` fallback survives only for a non-construct evidence shape
        // that stamps no status.
        let concluded = if is_construct {
            construct_conclusion(&bytes)
        } else {
            parse_status(&bytes).unwrap_or(exited_success)
        };
        // A passed construct-lane run's work is captured while its worktree still
        // exists (ADR-0152) — commit + tree recorded as correspondence rows, the
        // digest pair riding the evidence reference. Fail-closed: a passed run
        // whose capture falls short downgrades to a failing verdict rather than
        // admitting a pass whose work was lost with the worktree below.
        let candidate = if is_construct && concluded {
            self.capture_candidate(&worktree_dir, &handle.nonce)
        } else {
            None
        };
        let passed = concluded && (!is_construct || candidate.is_some());
        // The evidence has been consumed and any candidate captured — evict the
        // run so the registry tracks only in-flight orders rather than growing for
        // the process's lifetime, and reclaim its scratch worktree so the checkout
        // does not outlive the run. (The failed-read path above returns early,
        // keeping both the registry entry and the worktree for a later retry.)
        self.lock().remove(&handle.nonce.0);
        self.release_worktree(&worktree_dir);
        let verdict = if passed {
            StageVerdict::VerificationPassed
        } else {
            StageVerdict::VerificationFailed
        };
        // The detail digest is the content address of the evidence bytes — the
        // supporting artifact the verdict points at.
        let detail = Digest::of_wire_bytes(&bytes);
        let name = attempt_artifact_name(&handle.nonce, &subject, verdict, &detail);
        Ok(vec![EvidenceRef {
            name,
            nonce: handle.nonce.clone(),
            // The local lane holds evidence on disk, not in a numbered artifact
            // store, so there is no backend artifact id; the name carries the whole
            // claim and the size is the file's length.
            artifact_id: 0,
            size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            candidate,
            findings: parse_findings(&bytes),
        }])
    }
}

/// The production spawn seam: `git worktree add` the checkout, then spawn
/// `cargo xtask transform` in that worktree — the same two steps the wrapper
/// workflow runs, performed natively on the operator's machine.
pub struct ProcessTransformRunner;

impl TransformRunner for ProcessTransformRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        // Materialize the sealed checkout into a detached scratch worktree. `--force`
        // reclaims a stale worktree dir left by a prior aborted run at the same nonce.
        let checkout = Command::new("git")
            .args(["worktree", "add", "--force", "--detach"])
            .arg(spec.worktree_dir)
            .arg(spec.checkout_hex)
            .output()
            .map_err(LocalExecutorError::Spawn)?;
        if !checkout.status.success() {
            return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&checkout.stderr), 1000)));
        }
        // The scratch worktree is a full checkout carrying the repo's interactive
        // `.claude/settings.json` hooks, and the construct lane spawns headless
        // `claude` in it — so the SessionStart worktree-rebind hook would fire and
        // strand the candidate diff in a nested session worktree (#3632). Neutralize
        // the hooks the same way the headless action does
        // (`.github/actions/headless-claude/action.yml`): strip the `hooks` key and
        // mark the file skip-worktree so the edit neither shows as a candidate nor
        // can be committed.
        neutralize_hooks(spec.worktree_dir)?;
        // Spawn the same portable entrypoint the wrappers run, in the checked-out
        // worktree, under the ambient local `claude` auth (ADR-0150).
        let mut cargo = Command::new("cargo");
        cargo
            .current_dir(spec.worktree_dir)
            .args(["xtask", "transform", spec.command, "--out"])
            .arg(spec.evidence_dir)
            .args(["--nonce", spec.nonce]);
        if spec.command == CONSTRUCT_IMPLEMENT_COMMAND || spec.command == REVIEW_CRITIC_COMMAND {
            cargo.args(["--subject", spec.checkout_hex]);
            if let Some(model) = spec.model {
                cargo.args(["--model", model]);
            }
            if let Some(effort) = spec.effort {
                cargo.args(["--effort", effort]);
            }
            if let Some(task) = spec.task {
                cargo.args(["--task", task]);
            }
        }
        let child = cargo.spawn().map_err(LocalExecutorError::Spawn)?;
        Ok(Box::new(ChildProcess { child }))
    }

    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        // Tear the scratch worktree back down on the run's terminal path. `--force`
        // discards the run's working-tree changes (the candidate has already been
        // captured and read) and drops the admin entry `git worktree add` registered,
        // so a long-lived backend does not leak one worktree per order.
        let removed = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(worktree_dir)
            .output()
            .map_err(LocalExecutorError::Spawn)?;
        if !removed.status.success() {
            return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&removed.stderr), 1000)));
        }
        Ok(())
    }

    fn capture(&self, worktree_dir: &Path) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        // A clean worktree has nothing to capture — the caller fails the run
        // closed rather than minting an empty candidate.
        if git_in(worktree_dir, &["status", "--porcelain"])?.trim().is_empty() {
            return Ok(None);
        }
        git_in(worktree_dir, &["add", "--all"])?;
        // Commit under the bloomery's own fixed identity, in the host's trust
        // domain (ADR-0152: the child never stages, commits, or holds
        // credentials). `--no-verify` keeps repo hooks out of the capture path —
        // the run's own gates already judged the work.
        git_in(
            worktree_dir,
            &[
                "-c",
                "user.name=aether-bloomery",
                "-c",
                "user.email=bloomery@iamateapot.dev",
                "commit",
                "--no-verify",
                "--message",
                "bloomery: candidate capture",
            ],
        )?;
        let commit_hex = git_in(worktree_dir, &["rev-parse", "HEAD"])?;
        #[allow(clippy::literal_string_with_formatting_args, reason = "git revision syntax, not a format string")]
        let tree_hex = git_in(worktree_dir, &["rev-parse", "HEAD^{tree}"])?;
        let commit = GitObjectId::from_hex(commit_hex.trim()).ok_or_else(|| {
            LocalExecutorError::Worktree(format!("malformed capture commit sha `{}`", commit_hex.trim()))
        })?;
        let tree = GitObjectId::from_hex(tree_hex.trim())
            .ok_or_else(|| LocalExecutorError::Worktree(format!("malformed capture tree sha `{}`", tree_hex.trim())))?;
        Ok(Some(CapturedObjects { commit, tree }))
    }
}

// Run one git command inside `dir`, returning its stdout — the capture path's
// shell helper, error-shaped like the worktree add/remove shell-outs above.
fn git_in(dir: &Path, args: &[&str]) -> Result<String, LocalExecutorError> {
    let output = Command::new("git").current_dir(dir).args(args).output().map_err(LocalExecutorError::Spawn)?;
    if !output.status.success() {
        return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&output.stderr), 1000)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A live `cargo xtask transform` child.
struct ChildProcess {
    child: Child,
}

impl RunProcess for ChildProcess {
    fn poll(&mut self) -> RunLifecycle {
        match self.child.try_wait() {
            Ok(Some(status)) => RunLifecycle::Exited { success: status.success() },
            Ok(None) => RunLifecycle::Running,
            // A wait fault is not a live run; read it as a failed exit rather than
            // reporting an eternally-running child.
            Err(_) => RunLifecycle::Exited { success: false },
        }
    }

    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        self.child.kill().map_err(LocalExecutorError::Io)?;
        // Reap so the killed child does not linger as a zombie; a reap fault is a
        // real failure folded into the returned error, not silently swallowed.
        self.child.wait().map_err(LocalExecutorError::Io)?;
        Ok(())
    }
}

/// `Some(s)` for a non-empty config string, `None` for the empty default.
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// The last `max` bytes of `s`, snapped forward to a char boundary — a bounded
/// stderr tail for a worktree failure.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_owned()
}

/// Strip the top-level `hooks` key from a `.claude/settings.json` body, returning
/// the re-serialized JSON (pretty, trailing newline, matching the checked-in
/// shape). The pure core of the interactive-hook neutralization (#3632) —
/// unit-tested in isolation; a body with no `hooks` key round-trips unchanged in
/// content, malformed JSON is an `Err`.
fn strip_hooks(settings: &str) -> Result<String, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(settings)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("hooks");
    }
    let mut rendered = serde_json::to_string_pretty(&value)?;
    rendered.push('\n');
    Ok(rendered)
}

/// Neutralize the interactive-session `.claude/settings.json` hooks in the scratch
/// worktree (#3632): strip the `hooks` key through [`strip_hooks`] and mark the
/// file skip-worktree so the edit neither reads as a candidate change nor can be
/// committed. Absence of `.claude/settings.json` is a clean no-op (mirroring the
/// headless action's absence handling); a read, parse, or write fault surfaces as
/// an error rather than silently proceeding with live hooks.
fn neutralize_hooks(worktree_dir: &Path) -> Result<(), LocalExecutorError> {
    let settings_path = worktree_dir.join(".claude/settings.json");
    let body = match fs::read_to_string(&settings_path) {
        Ok(body) => body,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(LocalExecutorError::Io(error)),
    };
    let stripped = strip_hooks(&body).map_err(|error| LocalExecutorError::Io(io::Error::other(error)))?;
    fs::write(&settings_path, stripped).map_err(LocalExecutorError::Io)?;
    // Mark the stripped file skip-worktree so it stays out of the scratch-root
    // `git status --porcelain` the candidate detection reads (#3632) and can never
    // be committed as part of a candidate.
    let marked = Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .args(["update-index", "--skip-worktree", ".claude/settings.json"])
        .output()
        .map_err(LocalExecutorError::Spawn)?;
    if !marked.status.success() {
        return Err(LocalExecutorError::Worktree(tail(&String::from_utf8_lossy(&marked.stderr), 1000)));
    }
    Ok(())
}

/// Read the verify lane's `status` field from an `evidence.json` byte string:
/// `Some(true)` for `"pass"`, `Some(false)` for `"fail"`, `None` when the field
/// is absent (the construct lane's record carries no status) or the bytes are
/// not a decodable object.
fn parse_status(bytes: &[u8]) -> Option<bool> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match value.get("status").and_then(serde_json::Value::as_str)? {
        "pass" => Some(true),
        "fail" => Some(false),
        _ => None,
    }
}

// The evidence's top-level `findings` prose — what the review critic stamped
// (#3656), threaded onto a later Refine re-entry. Presence-driven: a lane that
// stamps none yields `None`, no lane flag needed.
fn parse_findings(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value.get("findings").and_then(serde_json::Value::as_str).map(str::to_owned)
}

/// Whether a construct lane's `evidence.json` byte string shows a **substantive
/// conclusion** (#3596): the run reached a terminal `result` with
/// `is_error == false` *and* left a candidate change in the working tree
/// (`produced_candidate == true`). The construct lane's whole job is to produce a
/// focused candidate change, so a run that merely exited zero with nothing to
/// review must not advance the member. Fail-closed — a `no_result` record (a run
/// that died early), an errored run (`is_error == true`), an empty candidate
/// (`produced_candidate` absent or `false`), or bytes that do not decode all
/// return `false`.
fn construct_conclusion(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let produced_candidate = value.get("produced_candidate").and_then(serde_json::Value::as_bool).unwrap_or(false);
    // A terminal `result` with is_error == false is the "the run concluded"
    // signal; a `no_result` record carries no `is_error` field, and an errored run
    // carries `is_error == true` — both fail this test.
    let concluded =
        value.get("result_record").and_then(|record| record.get("is_error")).and_then(serde_json::Value::as_bool)
            == Some(false);
    concluded && produced_candidate
}

/// A deterministic spawn seam for tests: writes a fixed `evidence.json` into the
/// run's output dir and hands back a process pinned to a fixed lifecycle — the
/// whole seam, without a real repo or Claude credential. Shared by this module's
/// unit tests and the executor-reactor runtime test.
#[cfg(test)]
pub mod testing {
    use std::fs;
    use std::path::Path;

    use aether_bloomery_github::GitObjectId;

    use super::{CapturedObjects, LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner};

    /// The canned capture a [`FixedRunner`] with `captures: true` returns: a
    /// fixed commit/tree object pair, so a test can assert the digests the
    /// backend derives and records from it.
    #[must_use]
    pub fn canned_capture() -> CapturedObjects {
        CapturedObjects {
            commit: GitObjectId::from_hex(&"c".repeat(40)).expect("40 hex chars is a well-formed sha1"),
            tree: GitObjectId::from_hex(&"d".repeat(40)).expect("40 hex chars is a well-formed sha1"),
        }
    }

    /// A runner that writes `evidence` and returns a process fixed at `lifecycle`.
    pub struct FixedRunner {
        /// The `evidence.json` bytes every run writes.
        pub evidence: String,
        /// The lifecycle every spawned process reports.
        pub lifecycle: RunLifecycle,
        /// Whether `capture` returns the [`canned_capture`] pair (`true`) or a
        /// clean-worktree `None` (`false`).
        pub captures: bool,
    }

    impl TransformRunner for FixedRunner {
        fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
            fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
            fs::write(spec.evidence_dir.join("evidence.json"), &self.evidence).map_err(LocalExecutorError::Io)?;
            Ok(Box::new(FixedProcess { lifecycle: self.lifecycle }))
        }

        // The stub never materializes a real worktree (`start` writes only the
        // evidence dir), so there is nothing to tear down.
        fn release(&self, _worktree_dir: &Path) -> Result<(), LocalExecutorError> {
            Ok(())
        }

        fn capture(&self, _worktree_dir: &Path) -> Result<Option<CapturedObjects>, LocalExecutorError> {
            Ok(self.captures.then(canned_capture))
        }
    }

    struct FixedProcess {
        lifecycle: RunLifecycle,
    }

    impl RunProcess for FixedProcess {
        fn poll(&mut self) -> RunLifecycle {
            self.lifecycle
        }

        fn kill(&mut self) -> Result<(), LocalExecutorError> {
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
