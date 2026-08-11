//! The local-process executor backend: an in-process registry of tracked runs
//! over the [`TransformRunner`] spawn seam, and its [`ExecutorBackend`] impl.

use std::collections::HashMap;
use std::path::{Path, PathBuf, absolute};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aether_bloomery::digest::ContentAddressed;
use aether_bloomery::{
    CandidateRef, Conclusion, Digest, EvidenceRef, ExecutionStatus, ExecutorBackend, Nonce, StageVerdict, StudyCost,
    VerifyFailureSet, WorkHandle, WorkOrder, digest_of, is_model_lane,
};
use aether_bloomery_github::{GitObjectId, SharedCorrespondence, parse_study_cost};
use serde::Serialize;
use std::fs;

use super::error::LocalExecutorError;
use super::lane_program::LaneProgram;
use super::process_runner::{CaptureIdentity, ProcessTransformRunner};
use super::runner::{RunLifecycle, RunProcess, RunSpec, TransformRunner};
use crate::bloomery::CONSTRUCT_IMPLEMENT_COMMAND;
use crate::bloomery::intake::NameEvidenceClaims;
use crate::bloomery::mirror::GithubMirrorConfig;

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
    // Whether the evidence body belongs to a mechanical verify lane and must
    // decode ADR-0178's `failed_verifiers` field.
    is_verify: bool,
}

/// The local-process executor backend: an in-process registry of tracked runs
/// keyed by nonce, over a [`TransformRunner`] spawn seam.
pub struct LocalExecutor {
    runner: Arc<dyn TransformRunner>,
    correspondence: SharedCorrespondence,
    base_dir: PathBuf,
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
    ) -> Self {
        Self { runner, correspondence, base_dir: base_dir.into(), runs: Mutex::new(HashMap::new()) }
    }

    /// Build the production backend from resolved config: the real git + cargo
    /// [`ProcessTransformRunner`], the shared `correspondence` the checkout
    /// resolves through, and the config'd scratch-worktree base dir. The model a
    /// run executes under is not config — it rides each order as the resolved
    /// agent profile the host overlaid at dispatch (ADR-0149 §The line).
    #[must_use]
    pub fn from_config(config: &GithubMirrorConfig, correspondence: SharedCorrespondence) -> Self {
        let identity = CaptureIdentity { name: config.operator_name.clone(), email: config.operator_email.clone() };

        Self::new(
            Arc::new(ProcessTransformRunner::new(identity, LaneProgram::parse(&config.local_lane_program))),
            correspondence,
            config.local_worktree_base.clone(),
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
        // The diff source rides the work order (#4723) and resolves the same way:
        // an order that names one is judged over the range `base..checkout`, one
        // that does not is judged over the working tree. Refused when it does not
        // resolve rather than silently omitted — the omission is invisible at the
        // lane, which then reads an empty working-tree diff as an empty candidate.
        let diff_base_hex = order
            .transformation
            .diff_base
            .map(|base| {
                self.correspondence
                    .resolve_git(&base)?
                    .ok_or_else(|| LocalExecutorError::UnresolvedDiffBase(order.nonce.clone()))
                    .map(|object| object.to_hex())
            })
            .transpose()?;
        // The subject the returning evidence binds to is the order's subject input
        // (the scope-revision digest the broker displayed), falling back to the
        // checkout only for a malformed order that carries no input.
        let subject = order.transformation.inputs.first().copied().unwrap_or(order.transformation.checkout);
        // Harness/model/effort/task ride the model-driven lanes (construct and
        // the review critic), mirroring `transform-model.yml`'s argv; a verify
        // lane ignores them. `is_construct` stays narrower — it selects the
        // construct-specific evidence gate (substantive-conclusion, #3596),
        // which the review lane's `status`-stamped evidence must not ride.
        let is_construct = order.transformation.command == CONSTRUCT_IMPLEMENT_COMMAND;
        let is_verify = order.transformation.command.starts_with("verify.");
        let is_model_lane = is_model_lane(&order.transformation.command);
        // The stage's resolved agent profile, overlaid onto the order by the
        // dispatching host (ADR-0149 §The line) — never a backend-local config
        // knob, which would let a run's model diverge from the profile its bloom
        // sealed and the receipt attests. An order that carries none names no
        // model, and the child falls back to the operator's ambient default.
        let profile = is_model_lane.then_some(order.transformation.model.as_ref()).flatten();
        let spec = RunSpec {
            command: &order.transformation.command,
            checkout_hex: &checkout_hex,
            diff_base_hex: diff_base_hex.as_deref(),
            worktree_dir: &worktree_dir,
            evidence_dir: &evidence_dir,
            nonce: &nonce,
            harness: profile.map(|resolved| resolved.harness.as_str()),
            model: profile.map(|resolved| resolved.model.as_str()),
            effort: profile.map(|resolved| resolved.effort.as_str()),
            // The work-order description rides the order's transformation (#3595),
            // populated at dispatch from durable state; the model lanes name it
            // (the critic judges the candidate against it), mirroring the
            // model/effort gate.
            task: is_model_lane.then_some(order.transformation.description.as_deref()).flatten(),
        };
        let process = self.runner.start(&spec)?;
        self.lock().insert(nonce, Run { process, worktree_dir, evidence_dir, subject, is_construct, is_verify });
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
        let (evidence_dir, subject, lifecycle, is_construct, is_verify, worktree_dir) = {
            let mut runs = self.lock();
            let Some(run) = runs.get_mut(&handle.nonce.0) else {
                return Err(LocalExecutorError::NoRunForNonce(handle.nonce.clone()));
            };
            (
                run.evidence_dir.clone(),
                run.subject,
                run.process.poll(),
                run.is_construct,
                run.is_verify,
                run.worktree_dir.clone(),
            )
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
                        name: NameEvidenceClaims::attempt_artifact_name(
                            &handle.nonce,
                            &subject,
                            StageVerdict::VerificationFailed,
                            VerifyFailureSet::EMPTY,
                            &Digest::of_wire_bytes(&[]),
                        ),
                        nonce: handle.nonce.clone(),
                        artifact_id: 0,
                        size_bytes: 0,
                        candidate: None,
                        findings: None,
                        failed_verifiers: VerifyFailureSet::EMPTY,
                        // Synthesized, not reported: there are no evidence bytes
                        // to read a cost out of, so the attempt is unmeasured.
                        cost: None,
                    }]);
                }
                return Err(LocalExecutorError::Evidence(format!("{}: {read_error}", evidence_path.display())));
            }
        };
        // Evidence must identify the order that produced it before any body claim
        // is trusted. A stale or cross-wired evidence directory is otherwise able
        // to advance a different order merely by carrying a passing verdict.
        let nonce_matches = evidence_nonce_matches(&bytes, &handle.nonce);
        let failed_verifiers = if is_verify && nonce_matches {
            parse_failed_verifiers(&bytes)
        } else {
            Some(VerifyFailureSet::EMPTY)
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
            nonce_matches && construct_conclusion(&bytes)
        } else if is_verify {
            nonce_matches && failed_verifiers.is_some() && parse_status(&bytes).unwrap_or(exited_success)
        } else {
            nonce_matches && parse_status(&bytes).unwrap_or(exited_success)
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
        let failed_verifiers = failed_verifiers.unwrap_or_default();
        let name =
            NameEvidenceClaims::attempt_artifact_name(&handle.nonce, &subject, verdict, failed_verifiers, &detail);
        Ok(vec![EvidenceRef {
            name,
            nonce: handle.nonce.clone(),
            // The local lane holds evidence on disk, not in a numbered artifact
            // store, so there is no backend artifact id; the name carries the whole
            // claim and the size is the file's length.
            artifact_id: 0,
            size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            candidate,
            findings: nonce_matches.then(|| parse_findings(&bytes)).flatten(),
            failed_verifiers,
            cost: nonce_matches.then(|| parse_cost(&bytes)).flatten(),
        }])
    }
}

/// Whether `bytes` carry a top-level nonce that decodes as an executor handle
/// and names exactly `expected`. Evidence bodies are untrusted until this binds
/// them to the registry entry that supplied their directory.
fn evidence_nonce_matches(bytes: &[u8], expected: &Nonce) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let Some(nonce) = value.get("nonce").filter(|nonce| nonce.is_string()) else {
        return false;
    };
    serde_json::from_value::<Nonce>(nonce.clone()).is_ok_and(|actual| actual == *expected)
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

/// Decode the optional body-derived ADR-0178 failure set. Absence is the valid
/// empty/pass representation; a present malformed or noncanonical value is an
/// invalid body (`None`) and makes the local verdict fail closed.
fn parse_failed_verifiers(bytes: &[u8]) -> Option<VerifyFailureSet> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value
        .get("failed_verifiers")
        .map_or(Some(VerifyFailureSet::EMPTY), |failures| serde_json::from_value(failures.clone()).ok())
}

// The evidence's top-level `findings` prose — what the review critic stamped
// (#3656), threaded onto a later Refine re-entry. Presence-driven: a lane that
// stamps none yields `None`, no lane flag needed.
fn parse_findings(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value.get("findings").and_then(serde_json::Value::as_str).map(str::to_owned)
}

/// What the attempt cost, from the `result_record` the lane nested in its
/// `evidence.json` (#4679) — the same object `construct_conclusion` reads
/// `is_error` out of, parsed for its token and price columns instead.
///
/// Presence-driven like [`parse_findings`], and `None` at every shortfall: a
/// lane that nests no record, bytes that do not decode, or a record whose
/// columns do not parse. `None` means *unmeasured* and writes no study row —
/// the alternative, a row of zeroes, would make an unmeasured attempt
/// indistinguishable from a free one and quietly corrupt every average taken
/// over the ledger.
fn parse_cost(bytes: &[u8]) -> Option<StudyCost> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    parse_study_cost(&serde_json::to_vec(value.get("result_record")?).ok()?).ok()
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
