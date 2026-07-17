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
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::{fs, io};

use aether_bloomery::{
    Conclusion, Digest, EvidenceRef, ExecutionStatus, ExecutorBackend, Nonce, WorkHandle, WorkOrder,
};
use aether_bloomery_github::StageVerdict;

use super::CONSTRUCT_IMPLEMENT_COMMAND;
use super::intake::attempt_artifact_name;

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
        }
    }
}

impl Error for LocalExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Io(error) => Some(error),
            Self::Worktree(_) | Self::Evidence(_) | Self::NoRunForNonce(_) => None,
        }
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
}

/// One tracked run: the spawned child, where its evidence lands, and the digest
/// the returning evidence must bind to.
struct Run {
    process: Box<dyn RunProcess>,
    evidence_dir: PathBuf,
    // The digest the intake broker binds the evidence to — the order's subject
    // input (`transformation.inputs[0]`, what `drain_and_dispatch` records as the
    // displayed digest), NOT the checkout target. The two are distinct axes: the
    // checkout is the tree the work runs on, the subject is what the evidence is
    // about. Binding to the checkout would refuse at intake as a digest mismatch.
    subject: Digest,
}

/// The local-process executor backend: an in-process registry of tracked runs
/// keyed by nonce, over a [`TransformRunner`] spawn seam.
pub struct LocalExecutor {
    runner: Arc<dyn TransformRunner>,
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
        base_dir: impl Into<PathBuf>,
        construct_model: Option<String>,
        construct_effort: Option<String>,
    ) -> Self {
        Self { runner, base_dir: base_dir.into(), construct_model, construct_effort, runs: Mutex::new(HashMap::new()) }
    }

    /// Build the production backend from resolved config: the real git + cargo
    /// [`ProcessTransformRunner`], the config'd scratch-worktree base dir, and the
    /// config'd construct model/effort (empty strings resolve to `None`).
    #[must_use]
    pub fn from_config(config: &super::mirror::GithubMirrorConfig) -> Self {
        Self::new(
            Arc::new(ProcessTransformRunner),
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
}

impl ExecutorBackend for LocalExecutor {
    type Error = LocalExecutorError;

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        let nonce = order.nonce.0.clone();
        let worktree_dir = self.base_dir.join(&nonce);
        let evidence_dir = self.base_dir.join(format!("{nonce}-evidence"));
        let checkout_hex = hex_of(&order.transformation.checkout);
        // The subject the returning evidence binds to is the order's subject input
        // (the scope-revision digest the broker displayed), falling back to the
        // checkout only for a malformed order that carries no input.
        let subject = order.transformation.inputs.first().copied().unwrap_or(order.transformation.checkout);
        // Model/effort ride only the model-driven construct lane, mirroring
        // `transform-model.yml`'s argv; a verify lane ignores them.
        let is_construct = order.transformation.command == CONSTRUCT_IMPLEMENT_COMMAND;
        let spec = RunSpec {
            command: &order.transformation.command,
            checkout_hex: &checkout_hex,
            worktree_dir: &worktree_dir,
            evidence_dir: &evidence_dir,
            nonce: &nonce,
            model: is_construct.then_some(self.construct_model.as_deref()).flatten(),
            effort: is_construct.then_some(self.construct_effort.as_deref()).flatten(),
        };
        let process = self.runner.start(&spec)?;
        self.lock().insert(nonce, Run { process, evidence_dir, subject });
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
        let mut runs = self.lock();
        let Some(run) = runs.get_mut(&handle.nonce.0) else {
            return Err(LocalExecutorError::NoRunForNonce(handle.nonce.clone()));
        };
        run.process.kill()?;
        // A cancel is terminal — evict the killed run so the registry tracks only
        // in-flight orders rather than parking `cancelled` entries forever.
        runs.remove(&handle.nonce.0);
        Ok(())
    }

    #[allow(clippy::significant_drop_tightening, reason = "run is a &mut reborrow; the guard must outlive it")]
    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        // Pull the run's on-disk location, binding digest, and terminal exit out of
        // the guarded region, then drop the lock — the evidence read is blocking IO
        // and must not hold the registry mutex.
        let (evidence_dir, subject, exited_success) = {
            let mut runs = self.lock();
            let Some(run) = runs.get_mut(&handle.nonce.0) else {
                return Err(LocalExecutorError::NoRunForNonce(handle.nonce.clone()));
            };
            (
                run.evidence_dir.clone(),
                run.subject,
                matches!(run.process.poll(), RunLifecycle::Exited { success: true }),
            )
        };
        let evidence_path = evidence_dir.join("evidence.json");
        let bytes = fs::read(&evidence_path)
            .map_err(|error| LocalExecutorError::Evidence(format!("{}: {error}", evidence_path.display())))?;
        // The evidence has been consumed — evict the run so the registry tracks
        // only in-flight orders rather than growing for the process's lifetime. A
        // failed read leaves the entry above so a later cycle can retry it.
        self.lock().remove(&handle.nonce.0);
        // Verdict from the run's own evidence: the verify lane stamps a `status`
        // ("pass"/"fail"); the construct lane's record carries none, so its verdict
        // folds from the child's terminal exit. A passing outcome maps to
        // VerificationPassed — the only non-approval verdict the completion gate
        // reads as passing (`verdict_passed`), so a produced construct candidate
        // advances the member.
        let passed = parse_status(&bytes).unwrap_or(exited_success);
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
        // Spawn the same portable entrypoint the wrappers run, in the checked-out
        // worktree, under the ambient local `claude` auth (ADR-0150).
        let mut cargo = Command::new("cargo");
        cargo
            .current_dir(spec.worktree_dir)
            .args(["xtask", "transform", spec.command, "--out"])
            .arg(spec.evidence_dir)
            .args(["--nonce", spec.nonce]);
        if spec.command == CONSTRUCT_IMPLEMENT_COMMAND {
            cargo.args(["--subject", spec.checkout_hex]);
            if let Some(model) = spec.model {
                cargo.args(["--model", model]);
            }
            if let Some(effort) = spec.effort {
                cargo.args(["--effort", effort]);
            }
        }
        let child = cargo.spawn().map_err(LocalExecutorError::Spawn)?;
        Ok(Box::new(ChildProcess { child }))
    }
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

/// Lowercase-hex a digest's 32 bytes — the render `git` and the `--subject` argv
/// take the checkout target as.
fn hex_of(digest: &Digest) -> String {
    let mut hex = String::with_capacity(64);
    for byte in digest.as_bytes() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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

/// A deterministic spawn seam for tests: writes a fixed `evidence.json` into the
/// run's output dir and hands back a process pinned to a fixed lifecycle — the
/// whole seam, without a real repo or Claude credential. Shared by this module's
/// unit tests and the executor-driver runtime test.
#[cfg(test)]
pub mod testing {
    use std::fs;

    use super::{LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner};

    /// A runner that writes `evidence` and returns a process fixed at `lifecycle`.
    pub struct FixedRunner {
        /// The `evidence.json` bytes every run writes.
        pub evidence: String,
        /// The lifecycle every spawned process reports.
        pub lifecycle: RunLifecycle,
    }

    impl TransformRunner for FixedRunner {
        fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
            fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
            fs::write(spec.evidence_dir.join("evidence.json"), &self.evidence).map_err(LocalExecutorError::Io)?;
            Ok(Box::new(FixedProcess { lifecycle: self.lifecycle }))
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
