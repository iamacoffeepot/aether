//! The local backend's registry / lifecycle / evidence-synthesis logic, over a
//! stub [`TransformRunner`] that writes a canned output dir — no real git repo,
//! no Claude credential. The decisive property: the synthesized [`EvidenceRef`]
//! round-trips through [`NameEvidenceClaims`], so an admitted local run binds
//! exactly as a wrapper-uploaded one would.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aether_bloomery::{
    BackendObjectId, Conclusion, Correspondence, CorrespondenceError, Digest, ExecutionStatus, ExecutorBackend,
    Harness, Nonce, ReasoningEffort, ResolvedModel, StageCatalog, StageId, StageVerdict, Transformation, VerifyFailure,
    VerifyFailureSet, WorkHandle,
};
use tempfile::TempDir;

use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::to_hex;

use super::runner::CapturedObjects;
use super::testing::{FixedRunner, canned_capture};
use super::{LocalExecutor, LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner};
use crate::bloomery::intake::{EvidenceClaims, NameEvidenceClaims};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

// A correspondence seeded with the one checkout target these orders carry
// (`for_member_stage`'s third arg, `digest(0xC0)`), so `submit` resolves the
// `git worktree add` target through it rather than hex-punning the digest.
fn correspondence() -> Arc<FakeGithub> {
    let fake = FakeGithub::new();
    fake.seed_git_object(&digest(0xC0));
    Arc::new(fake)
}

// The local backend's `Nonce` is a work-order correlation id, not a cryptographic
// nonce; deriving a test nonce off a tag here keeps the CodeQL
// hard-coded-crypto-value scan (a false positive on this correlation-id type) from
// tripping on the call sites (#3596 review).
fn test_nonce(tag: &str) -> String {
    format!("wo-{tag}")
}

fn executor(base: &TempDir, evidence: &str, lifecycle: RunLifecycle) -> LocalExecutor {
    let runner = FixedRunner { evidence: evidence.to_owned(), lifecycle, captures: true };
    LocalExecutor::new(Arc::new(runner), correspondence(), base.path())
}

fn construct_order(subject: Digest, nonce: &str) -> aether_bloomery::WorkOrder {
    aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Construct),
            subject,
            digest(0xC0),
        ),
        nonce: Nonce(nonce.to_owned()),
    }
}

#[test]
fn submit_inspect_stream_synthesizes_an_admissible_evidence_ref() {
    // A construct run that concluded substantively — a terminal `result` with
    // is_error == false and a produced candidate — folds to VerificationPassed so
    // it advances the member (#3596). The synthesized ref must round-trip through
    // NameEvidenceClaims with the subject bound to the order's subject input, the
    // digest intake binds.
    let base = TempDir::new().unwrap();
    let subject = digest(5);
    let evidence = r#"{"command":"construct.implement","nonce":"n-1","produced_candidate":true,"result_record":{"schema":1,"is_error":false,"result":{"num_turns":3}}}"#;
    let exec = executor(&base, evidence, RunLifecycle::Exited { success: true });

    let handle = exec.submit(&construct_order(subject, "n-1")).unwrap();
    assert_eq!(handle, WorkHandle::new(Nonce("n-1".to_owned())));
    assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Completed { conclusion: Conclusion::Success });

    let refs = exec.stream_evidence(&handle).unwrap();
    assert_eq!(refs.len(), 1, "one evidence ref per local run");
    assert!(refs[0].name.starts_with("attempt."), "the ref name is the attempt-artifact contract");
    assert_eq!(refs[0].nonce, Nonce("n-1".to_owned()));

    // The decisive round-trip: the pull side decodes it, and the subject is the
    // order's subject input — not the checkout target — so the broker binds it.
    let upload = NameEvidenceClaims.claim_for(&refs[0]).expect("the synthesized ref decodes as an attempt result");
    assert_eq!(upload.nonce, Nonce("n-1".to_owned()));
    assert_eq!(upload.subject, subject, "the evidence binds to the order's subject input, not the checkout");
    assert_eq!(
        upload.verdict,
        StageVerdict::VerificationPassed,
        "a substantive construct conclusion folds to a passing verdict"
    );
    assert_eq!(upload.detail, Digest::of_wire_bytes(evidence.as_bytes()), "the detail is the evidence content address");
}

#[test]
fn a_verify_status_field_drives_the_verdict() {
    // The verify lane stamps a `status`; a "fail" status yields VerificationFailed
    // regardless of the (stubbed) exit, so the name-encoded verdict is the claim.
    let base = TempDir::new().unwrap();
    let subject = digest(7);
    let exec = executor(
        &base,
        r#"{"command":"verify.check","nonce":"n-v","status":"fail","failed_verifiers":["verify.fmt","verify.test"]}"#,
        RunLifecycle::Exited { success: true },
    );

    let order = aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Verify),
            subject,
            digest(0xC0),
        ),
        nonce: Nonce("n-v".to_owned()),
    };
    let handle = exec.submit(&order).unwrap();
    let refs = exec.stream_evidence(&handle).unwrap();

    let upload = NameEvidenceClaims.claim_for(&refs[0]).unwrap();
    assert_eq!(
        upload.verdict,
        StageVerdict::VerificationFailed,
        "the evidence status, not the exit, drives the verdict"
    );
    assert_eq!(upload.subject, subject);
    let expected = [VerifyFailure::Fmt, VerifyFailure::Test].into_iter().collect::<VerifyFailureSet>();
    assert_eq!(refs[0].failed_verifiers, expected, "the reference carries the body-derived canonical set");
    assert_eq!(upload.failed_verifiers, expected, "the body-derived set reaches the upload through the name");
}

#[test]
fn a_passing_verify_body_projects_the_empty_failure_set() {
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"verify.check","nonce":"n-pass","status":"pass"}"#;
    let exec = executor(&base, evidence, RunLifecycle::Exited { success: true });
    let order = aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Verify),
            digest(7),
            digest(0xC0),
        ),
        nonce: Nonce("n-pass".to_owned()),
    };

    let reference = exec.stream_evidence(&exec.submit(&order).unwrap()).unwrap().remove(0);
    let upload = NameEvidenceClaims.claim_for(&reference).expect("canonical local name decodes");

    assert!(reference.failed_verifiers.is_empty());
    assert!(upload.failed_verifiers.is_empty());
    assert_eq!(upload.verdict, StageVerdict::VerificationPassed);
}

#[test]
fn a_malformed_body_failure_set_fails_closed() {
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"verify.check","nonce":"n-bad-set","status":"pass","failed_verifiers":["verify.test","verify.fmt"]}"#;
    let exec = executor(&base, evidence, RunLifecycle::Exited { success: true });
    let order = aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Verify),
            digest(7),
            digest(0xC0),
        ),
        nonce: Nonce("n-bad-set".to_owned()),
    };

    let reference = exec.stream_evidence(&exec.submit(&order).unwrap()).unwrap().remove(0);
    let upload = NameEvidenceClaims.claim_for(&reference).expect("the fail-closed empty-mask name decodes");

    assert_eq!(upload.verdict, StageVerdict::VerificationFailed);
    assert!(upload.failed_verifiers.is_empty(), "invalid body data is never projected as a typed claim");
}

#[test]
fn inspect_is_unknown_for_an_untracked_nonce() {
    let base = TempDir::new().unwrap();
    let exec = executor(&base, "{}", RunLifecycle::Running);
    // Never submitted here: the clean Unknown, not an error.
    assert_eq!(exec.inspect(&WorkHandle::new(Nonce("ghost".to_owned()))).unwrap(), ExecutionStatus::Unknown);
}

#[test]
fn cancel_evicts_the_tracked_run() {
    let base = TempDir::new().unwrap();
    let exec = executor(&base, "{}", RunLifecycle::Running);
    let handle = exec.submit(&construct_order(digest(5), "n-c")).unwrap();
    assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Running);

    // A cancel kills the child and evicts the run — a subsequent inspect reports
    // the clean Unknown (never the killed child's exit as a plain completion), and
    // the registry no longer parks the terminal entry.
    exec.cancel(&handle).unwrap();
    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Unknown,
        "a cancelled run is evicted, so it reports Unknown rather than its exit"
    );
}

#[test]
fn stream_evidence_evicts_the_consumed_run() {
    // Once the evidence is read, the run is consumed: the registry drops it so a
    // long-lived backend tracks only in-flight orders, and a re-inspect of the
    // same handle reports the clean Unknown.
    let base = TempDir::new().unwrap();
    let exec = executor(&base, r#"{"command":"verify.check","status":"pass"}"#, RunLifecycle::Exited { success: true });
    let handle = exec.submit(&construct_order(digest(5), "n-e")).unwrap();

    exec.stream_evidence(&handle).unwrap();
    assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Unknown, "the consumed run is evicted after its read");
    match exec.stream_evidence(&handle) {
        Err(LocalExecutorError::NoRunForNonce(_)) => {}
        other => panic!("expected NoRunForNonce after eviction, got {other:?}"),
    }
}

// Fold the verdict a construct run's `evidence.json` yields through the whole
// `stream_evidence` path (the fixed-runner seam), decoded back off the
// synthesized ref — the gate's substantive-conclusion contract end-to-end
// (#3596). The stubbed child exits zero throughout, so a failing verdict proves
// the gate reads the evidence, not the child's exit.
fn construct_verdict(evidence: &str) -> StageVerdict {
    let base = TempDir::new().unwrap();
    let exec = executor(&base, evidence, RunLifecycle::Exited { success: true });
    let handle = exec.submit(&construct_order(digest(5), "n-g")).unwrap();
    let refs = exec.stream_evidence(&handle).unwrap();
    NameEvidenceClaims.claim_for(&refs[0]).expect("the synthesized ref decodes").verdict
}

#[test]
fn construct_gate_passes_a_substantive_conclusion() {
    // A terminal `result` with is_error == false AND a produced candidate — the
    // only shape that advances the member.
    let ev = r#"{"command":"construct.implement","nonce":"n-g","produced_candidate":true,"result_record":{"is_error":false,"result":{"num_turns":3}}}"#;
    assert_eq!(construct_verdict(ev), StageVerdict::VerificationPassed);
}

#[test]
fn evidence_for_a_different_nonce_fails_closed_before_its_claims_are_read() {
    let base = TempDir::new().unwrap();
    let expected = "wo-authoritative";
    let evidence = r#"{"command":"construct.implement","nonce":"wo-stale","produced_candidate":true,"findings":"do not trust this","result_record":{"schema":1,"is_error":false,"result":{"num_turns":3}}}"#;
    let exec = executor(&base, evidence, RunLifecycle::Exited { success: true });
    let handle = exec.submit(&construct_order(digest(5), expected)).unwrap();

    let refs = exec.stream_evidence(&handle).unwrap();
    let upload = NameEvidenceClaims.claim_for(&refs[0]).expect("the synthesized ref decodes");
    assert_eq!(upload.verdict, StageVerdict::VerificationFailed, "a stale body cannot advance this order");
    assert_eq!(refs[0].nonce, Nonce(expected.to_owned()), "the authoritative handle remains the claim nonce");
    assert_eq!(refs[0].size_bytes, u64::try_from(evidence.len()).unwrap());
    assert_eq!(upload.detail, Digest::of_wire_bytes(evidence.as_bytes()), "the raw evidence remains accountable");
    assert!(refs[0].candidate.is_none(), "a stale construct body cannot trigger capture");
    assert!(refs[0].findings.is_none(), "a stale body cannot direct a repair lap");
    assert!(refs[0].cost.is_none(), "a stale body cannot enter study accounting");
    assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Unknown, "the terminal stale body is consumed");
}

#[test]
fn construct_gate_fails_an_empty_candidate_run_despite_a_clean_exit() {
    // The exact 2026-07-17 bloom-trial bug: is_error == false and the child exits
    // zero, but the run left no candidate — nothing to review, so it must NOT
    // advance the member. The old `exited_success` fallthrough passed this.
    let ev = r#"{"command":"construct.implement","nonce":"n-g","produced_candidate":false,"result_record":{"is_error":false,"result":{"num_turns":6}}}"#;
    assert_eq!(construct_verdict(ev), StageVerdict::VerificationFailed);
}

#[test]
fn construct_gate_fails_an_errored_run_even_with_a_candidate() {
    let ev = r#"{"command":"construct.implement","nonce":"n-g","produced_candidate":true,"result_record":{"is_error":true,"result":{}}}"#;
    assert_eq!(construct_verdict(ev), StageVerdict::VerificationFailed);
}

#[test]
fn construct_gate_fails_a_dead_run_with_no_terminal_result() {
    // A `no_result` record (the run died before its terminal event) carries no
    // `is_error` field — fail-closed even with a candidate present.
    let ev = r#"{"command":"construct.implement","nonce":"n-g","produced_candidate":true,"result_record":{"no_result":true}}"#;
    assert_eq!(construct_verdict(ev), StageVerdict::VerificationFailed);
}

#[test]
fn construct_gate_fails_unparseable_evidence() {
    // Bytes that do not decode as a construct record are fail-closed — the run is
    // known to be the construct lane (the Run's flag), so the exit is never read.
    assert_eq!(construct_verdict("not json at all"), StageVerdict::VerificationFailed);
}

#[test]
fn cancel_and_stream_with_no_tracked_run_are_the_no_run_error() {
    let base = TempDir::new().unwrap();
    let exec = executor(&base, "{}", RunLifecycle::Running);
    let ghost = WorkHandle::new(Nonce("ghost".to_owned()));

    match exec.cancel(&ghost) {
        Err(LocalExecutorError::NoRunForNonce(nonce)) => assert_eq!(nonce, Nonce("ghost".to_owned())),
        other => panic!("expected NoRunForNonce, got {other:?}"),
    }
    match exec.stream_evidence(&ghost) {
        Err(LocalExecutorError::NoRunForNonce(_)) => {}
        other => panic!("expected NoRunForNonce, got {other:?}"),
    }
}

// A spawn seam that records the worktree dirs it is asked to release, and can be
// told to write no `evidence.json` (so a `stream_evidence` read faults) — the seam
// for asserting the local backend releases a run's scratch worktree on exactly its
// terminal paths (#3596 review, resource-leak finding).
struct RecordingRunner {
    // `None` → `start` writes no evidence.json, forcing a failed `stream_evidence` read.
    evidence: Option<String>,
    lifecycle: RunLifecycle,
    released: Arc<Mutex<Vec<PathBuf>>>,
}

impl TransformRunner for RecordingRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        if let Some(evidence) = &self.evidence {
            fs::write(spec.evidence_dir.join("evidence.json"), evidence).map_err(LocalExecutorError::Io)?;
        }
        Ok(Box::new(RecordingProcess { lifecycle: self.lifecycle }))
    }

    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        self.released.lock().unwrap().push(worktree_dir.to_owned());
        Ok(())
    }

    fn capture(&self, _worktree_dir: &Path) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        Ok(Some(canned_capture()))
    }
}

struct RecordingProcess {
    lifecycle: RunLifecycle,
}

impl RunProcess for RecordingProcess {
    fn poll(&mut self) -> RunLifecycle {
        self.lifecycle
    }

    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        Ok(())
    }
}

fn recording_executor(
    base: &TempDir,
    evidence: Option<&str>,
    lifecycle: RunLifecycle,
) -> (LocalExecutor, Arc<Mutex<Vec<PathBuf>>>) {
    let released = Arc::new(Mutex::new(Vec::new()));
    let runner = RecordingRunner { evidence: evidence.map(str::to_owned), lifecycle, released: Arc::clone(&released) };
    (LocalExecutor::new(Arc::new(runner), correspondence(), base.path()), released)
}

// What a `CapturingRunner` run saw: the `--out` path the child would resolve
// against, and the model/effort argv the lane would run under.
#[derive(Clone, Default)]
struct SeenSpec {
    evidence_dir: Option<PathBuf>,
    harness: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    checkout: Option<String>,
    diff_base: Option<String>,
}

// A spawn seam that records the `RunSpec` it was handed, so a test can assert what
// `submit` actually hands the child across seams the other stubs sidestep — the
// absolute `--out` path, and the resolved agent profile the order carries.
struct CapturingRunner {
    seen: Arc<Mutex<SeenSpec>>,
}

impl TransformRunner for CapturingRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        *self.seen.lock().unwrap() = SeenSpec {
            evidence_dir: Some(spec.evidence_dir.to_owned()),
            harness: spec.harness.map(str::to_owned),
            model: spec.model.map(str::to_owned),
            effort: spec.effort.map(str::to_owned),
            checkout: Some(spec.checkout_hex.to_owned()),
            diff_base: spec.diff_base_hex.map(str::to_owned),
        };
        Ok(Box::new(RecordingProcess { lifecycle: RunLifecycle::Running }))
    }

    fn release(&self, _worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        Ok(())
    }

    fn capture(&self, _worktree_dir: &Path) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        Ok(None)
    }
}

#[test]
fn submit_resolves_a_relative_base_to_an_absolute_evidence_dir() {
    // The child runs with `current_dir(worktree_dir)`, so a *relative* `--out`
    // resolves against the child's cwd (the scratch worktree) while `stream_evidence`
    // reads it against the coordinator's cwd — the two diverge and the intake polls a
    // path the run never wrote (the live 2026-07-18 bug). Tripwire: the evidence
    // out-path handed to the spawn must be absolute so the child writes where
    // `stream_evidence` reads, regardless of the coordinator's cwd or a relative
    // configured `local_worktree_base`.
    let seen = Arc::new(Mutex::new(SeenSpec::default()));
    let runner = CapturingRunner { seen: Arc::clone(&seen) };
    let exec = LocalExecutor::new(Arc::new(runner), correspondence(), PathBuf::from(".bloomery/local-worktrees"));

    exec.submit(&construct_order(digest(5), &test_nonce("abs"))).unwrap();

    let evidence_dir = seen.lock().unwrap().evidence_dir.clone().expect("submit spawned the run");
    assert!(
        evidence_dir.is_absolute(),
        "the evidence out-path handed to the spawn must be absolute, got {evidence_dir:?}"
    );
}

// Tripwire: the model lane's spawn runs under the agent profile the order carries,
// never a backend-local default. The backend used to read the model from its own
// config, whose empty default omitted `--model` entirely and silently handed the
// run to the operator's ambient model — so a bloom's sealed profile and the model
// that actually ran could differ while the receipt attested the sealed one
// (ADR-0149 §The line, #4324). Both axes are asserted: effort regressed the same
// way, as an env var the CLI does not read.
#[test]
fn submit_spawns_the_model_lane_under_the_orders_resolved_profile() {
    let seen = Arc::new(Mutex::new(SeenSpec::default()));
    let runner = CapturingRunner { seen: Arc::clone(&seen) };
    let base = TempDir::new().unwrap();
    let exec = LocalExecutor::new(Arc::new(runner), correspondence(), base.path());

    let mut order = construct_order(digest(5), &test_nonce("profile"));
    order.transformation.model = Some(ResolvedModel {
        harness: Harness::Muse,
        model: "claude-opus-4-8".to_owned(),
        effort: ReasoningEffort::XHigh,
    });
    exec.submit(&order).unwrap();

    let SeenSpec { harness, model, effort, .. } = seen.lock().unwrap().clone();
    assert_eq!(model.as_deref(), Some("claude-opus-4-8"), "the spawn names the order's resolved model");
    assert_eq!(effort.as_deref(), Some("xhigh"), "the spawn names the order's resolved effort tier");
    // The third axis (#4578): the harness the profile calibrated must reach the
    // spawn too, or the run silently executes under whichever CLI the worker
    // defaults to while the receipt attests the sealed profile — the same
    // divergence #4324 fixed for model and effort.
    assert_eq!(harness.as_deref(), Some("muse"), "the spawn names the order's resolved harness");
}

// The complement: an order carrying no resolved profile names neither flag, so the
// child falls back to the operator's ambient defaults rather than a fabricated one.
#[test]
fn submit_names_no_model_when_the_order_carries_no_profile() {
    let seen = Arc::new(Mutex::new(SeenSpec::default()));
    let runner = CapturingRunner { seen: Arc::clone(&seen) };
    let base = TempDir::new().unwrap();
    let exec = LocalExecutor::new(Arc::new(runner), correspondence(), base.path());

    exec.submit(&construct_order(digest(5), &test_nonce("ambient"))).unwrap();

    let SeenSpec { harness, model, effort, .. } = seen.lock().unwrap().clone();
    assert_eq!(model, None, "no resolved profile means no model is named");
    assert_eq!(effort, None, "no resolved profile means no effort is named");
    assert_eq!(harness, None, "no resolved profile means no harness is named either");
}

// Tripwire: the aggregate review's candidate is committed — its worker checks out
// the integration head and finds a clean tree — so the spawn must hand the lane
// the range to judge. Without it the critic reads an empty working-tree diff,
// which its own instructions make a mandatory finding, and no bloom can ever pass
// its aggregate review (#4723). The member lane is asserted alongside it: naming a
// range there would judge the sealed base's own history instead of the candidate.
//
// Both resolved objects are also asserted as the exact lowercase hex git resolves,
// which is the whole reason the backend renders opaque correspondence bytes at all:
// a mis-rendered sha (uppercase, or a swapped nibble) names an object `git worktree
// add` refuses, and every dispatch fails on a target that looks plausible in a log.
#[test]
fn an_aggregate_review_spawn_names_the_range_a_member_spawn_does_not() {
    let seen = Arc::new(Mutex::new(SeenSpec::default()));
    let store = FakeGithub::new();
    store.seed_git_object(&digest(0xC0));
    store.seed_git_object(&digest(0xBA));
    let store = Arc::new(store);
    let base = TempDir::new().unwrap();
    let exec =
        LocalExecutor::new(Arc::new(CapturingRunner { seen: Arc::clone(&seen) }), Arc::clone(&store) as _, base.path());

    let review = aether_bloomery::WorkOrder {
        transformation: Transformation::for_aggregate_review(
            &StageCatalog::binding_of(StageId::AggregateReview),
            digest(5),
            digest(0xC0),
            digest(0xBA),
        ),
        nonce: Nonce(test_nonce("aggregate")),
    };
    exec.submit(&review).unwrap();

    // `seed_git_object` records each digest against its own hex rendering, so
    // that rendering is the sha the spawn must name.
    let SeenSpec { checkout, diff_base, .. } = seen.lock().unwrap().clone();
    assert_eq!(
        diff_base.as_deref(),
        Some(to_hex(&digest(0xBA)).as_str()),
        "the spawn names the sealed base as the range the integration is judged over",
    );
    assert_eq!(
        checkout.as_deref(),
        Some(to_hex(&digest(0xC0)).as_str()),
        "the checkout target is the resolved object, rendered as the lowercase hex git takes",
    );

    exec.submit(&construct_order(digest(5), &test_nonce("member"))).unwrap();
    assert_eq!(seen.lock().unwrap().diff_base, None, "a member candidate is the working tree, not a range");
}

// The complement, fail-closed: a diff base that resolves to no git object must
// refuse the submit rather than spawn without it. A dropped base is invisible at
// the lane — it falls back to the working-tree contract and reports the empty
// diff as "no candidate", which reads as the bloom's fault rather than the host's.
#[test]
fn an_unresolvable_diff_base_refuses_the_submit() {
    let seen = Arc::new(Mutex::new(SeenSpec::default()));
    let store = FakeGithub::new();
    store.seed_git_object(&digest(0xC0));
    let base = TempDir::new().unwrap();
    let exec = LocalExecutor::new(Arc::new(CapturingRunner { seen }), Arc::new(store), base.path());

    let review = aether_bloomery::WorkOrder {
        transformation: Transformation::for_aggregate_review(
            &StageCatalog::binding_of(StageId::AggregateReview),
            digest(5),
            digest(0xC0),
            digest(0xBA),
        ),
        nonce: Nonce(test_nonce("unseeded")),
    };

    match exec.submit(&review) {
        Err(LocalExecutorError::UnresolvedDiffBase(nonce)) => assert_eq!(nonce.0, test_nonce("unseeded")),
        other => panic!("expected UnresolvedDiffBase, got {other:?}"),
    }
}

#[test]
fn cancel_releases_the_scratch_worktree() {
    // A cancel is terminal — the killed run's scratch worktree is torn down so a
    // long-lived backend does not leak one worktree per cancelled order.
    let base = TempDir::new().unwrap();
    let (exec, released) = recording_executor(&base, Some("{}"), RunLifecycle::Running);
    let nonce = test_nonce("cancel");
    let handle = exec.submit(&construct_order(digest(5), &nonce)).unwrap();

    exec.cancel(&handle).unwrap();
    assert_eq!(released.lock().unwrap().as_slice(), &[base.path().join(&nonce)], "cancel releases the run's worktree");
}

#[test]
fn a_consumed_evidence_read_releases_the_scratch_worktree() {
    // Reading the evidence consumes the run — its scratch worktree is released as
    // the run leaves the registry.
    let base = TempDir::new().unwrap();
    let ev = r#"{"command":"construct.implement","nonce":"wo-stream","produced_candidate":true,"result_record":{"is_error":false,"result":{}}}"#;
    let (exec, released) = recording_executor(&base, Some(ev), RunLifecycle::Exited { success: true });
    let nonce = test_nonce("stream");
    let handle = exec.submit(&construct_order(digest(5), &nonce)).unwrap();

    exec.stream_evidence(&handle).unwrap();
    assert_eq!(
        released.lock().unwrap().as_slice(),
        &[base.path().join(&nonce)],
        "a consumed evidence read releases the run's worktree",
    );
}

#[test]
fn a_failed_evidence_read_retains_the_worktree_for_retry() {
    // No evidence.json written yet AND the run is still Running → the missing file is
    // transient. The run stays tracked for a later retry (the entry is intentionally
    // kept), so its worktree must NOT be released — releasing it would strip the
    // checkout a retry needs. (An *Exited* run with no evidence is terminal instead —
    // see `an_exited_run_with_no_evidence_yields_a_failed_verdict_and_evicts`.)
    let base = TempDir::new().unwrap();
    let (exec, released) = recording_executor(&base, None, RunLifecycle::Running);
    let handle = exec.submit(&construct_order(digest(5), &test_nonce("retry"))).unwrap();

    assert!(matches!(exec.stream_evidence(&handle), Err(LocalExecutorError::Evidence(_))));
    assert!(released.lock().unwrap().is_empty(), "a retryable failed read must not release the worktree");
    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Running,
        "the still-running run is retained after a transient failed read",
    );
}

#[test]
fn an_exited_run_with_no_evidence_yields_a_failed_verdict_and_evicts() {
    // An Exited run that left no evidence.json will never produce one — re-driving
    // the read against it loops forever (the live 2026-07-18 bloom-trial bug). It is
    // terminal: `stream_evidence` synthesizes a fail-closed VerificationFailed attempt
    // (feeding the retry/wedge machinery), evicts the run, and releases its worktree —
    // rather than the eternal error re-drive.
    let base = TempDir::new().unwrap();
    let (exec, released) = recording_executor(&base, None, RunLifecycle::Exited { success: true });
    let nonce = test_nonce("exited-no-evidence");
    let handle = exec.submit(&construct_order(digest(5), &nonce)).unwrap();

    let refs = exec.stream_evidence(&handle).unwrap();
    assert_eq!(refs.len(), 1, "one synthesized failure ref for the evidence-less exit");
    let upload = NameEvidenceClaims.claim_for(&refs[0]).expect("the synthesized ref decodes as an attempt result");
    assert_eq!(upload.verdict, StageVerdict::VerificationFailed, "an exited run with no visible evidence fails closed");

    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Unknown,
        "the terminal evidence-less run is evicted, not retained for an eternal re-drive",
    );
    assert_eq!(
        released.lock().unwrap().as_slice(),
        &[base.path().join(&nonce)],
        "the terminal evidence-less run releases its scratch worktree",
    );
}

// ADR-0152 — a passing construct run's capture rides the evidence reference and
// both digests resolve through the correspondence to the captured git objects.
// Catches the gap this arc closes: the work being discarded with the worktree
// after being read as a boolean.
#[test]
fn a_passing_construct_run_captures_its_candidate() {
    let base = TempDir::new().unwrap();
    let store = correspondence();
    let evidence = r#"{"command":"construct.implement","nonce":"n-cap","produced_candidate":true,"result_record":{"schema":1,"is_error":false,"result":{"num_turns":3}}}"#;
    let runner = FixedRunner {
        evidence: evidence.to_owned(),
        lifecycle: RunLifecycle::Exited { success: true },
        captures: true,
    };
    let exec = LocalExecutor::new(Arc::new(runner), Arc::clone(&store) as _, base.path());

    let handle = exec.submit(&construct_order(digest(5), "n-cap")).unwrap();
    let refs = exec.stream_evidence(&handle).unwrap();

    let candidate = refs[0].candidate.expect("a passed construct run reports its capture");
    let captured = canned_capture();
    assert_eq!(
        store.resolve_backend_object(&candidate.tree).unwrap().as_ref(),
        Some(&captured.tree),
        "the tree digest resolves to the captured tree object",
    );
    assert_eq!(
        store.resolve_backend_object(&candidate.checkout).unwrap().as_ref(),
        Some(&captured.commit),
        "the checkout digest resolves to the capture commit",
    );
    assert_ne!(candidate.tree, candidate.checkout, "the two axes are domain-separated digests");
    let upload = NameEvidenceClaims.claim_for(&refs[0]).unwrap();
    assert_eq!(upload.verdict, StageVerdict::VerificationPassed);
    assert_eq!(upload.candidate, Some(candidate), "the claim carries the capture to the intake");
}

// ADR-0152 — fail-closed: a construct run that concluded substantively but whose
// capture found a clean worktree downgrades to a failing verdict instead of
// admitting a pass whose work was lost. Catches the inverted gate (trusting the
// child's produced_candidate stamp over the host's own capture).
#[test]
fn a_passing_construct_run_with_nothing_to_capture_fails_closed() {
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"construct.implement","nonce":"n-void","produced_candidate":true,"result_record":{"schema":1,"is_error":false,"result":{"num_turns":3}}}"#;
    let runner = FixedRunner {
        evidence: evidence.to_owned(),
        lifecycle: RunLifecycle::Exited { success: true },
        captures: false,
    };
    let exec = LocalExecutor::new(Arc::new(runner), correspondence(), base.path());

    let handle = exec.submit(&construct_order(digest(5), "n-void")).unwrap();
    let refs = exec.stream_evidence(&handle).unwrap();

    assert!(refs[0].candidate.is_none());
    let upload = NameEvidenceClaims.claim_for(&refs[0]).unwrap();
    assert_eq!(upload.verdict, StageVerdict::VerificationFailed, "a lost capture is a failed attempt");
}

// A correspondence whose reads work — so the submit resolves its checkout — but
// whose `record` always faults, standing in for a durable store that goes
// unwritable between the dispatch and the capture.
struct RecordFaults(FakeGithub);

impl Correspondence for RecordFaults {
    fn record(&self, _digest: &Digest, _object: &BackendObjectId) -> Result<(), CorrespondenceError> {
        Err(CorrespondenceError::new("store fault"))
    }

    fn resolve_backend_object(&self, digest: &Digest) -> Result<Option<BackendObjectId>, CorrespondenceError> {
        self.0.resolve_backend_object(digest)
    }

    fn resolve_digest(&self, object: &BackendObjectId) -> Result<Option<Digest>, CorrespondenceError> {
        self.0.resolve_digest(object)
    }
}

// ADR-0152 — the other capture shortfall: the run committed real work, but the
// correspondence write faulted, so nothing can ever resolve the captured objects
// back. Fail-closed like a clean worktree, because a pass carrying a candidate
// the integrate path cannot resolve wedges the bloom one stage later with no
// trace of why. Catches a `record` fault folded to a warn while the verdict sails
// through on the child's own claim.
#[test]
fn a_capture_whose_correspondence_write_faults_fails_closed() {
    let base = TempDir::new().unwrap();
    let store = FakeGithub::new();
    store.seed_git_object(&digest(0xC0));
    let evidence = r#"{"command":"construct.implement","nonce":"n-fault","produced_candidate":true,"result_record":{"schema":1,"is_error":false,"result":{"num_turns":3}}}"#;
    let runner = FixedRunner {
        evidence: evidence.to_owned(),
        lifecycle: RunLifecycle::Exited { success: true },
        captures: true,
    };
    let exec = LocalExecutor::new(Arc::new(runner), Arc::new(RecordFaults(store)), base.path());

    let handle = exec.submit(&construct_order(digest(5), "n-fault")).unwrap();
    let refs = exec.stream_evidence(&handle).unwrap();

    assert!(refs[0].candidate.is_none(), "an unrecordable capture carries no candidate");
    let upload = NameEvidenceClaims.claim_for(&refs[0]).unwrap();
    assert_eq!(upload.verdict, StageVerdict::VerificationFailed, "an unrecordable capture is a failed attempt");
}
