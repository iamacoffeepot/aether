//! The local backend's registry / lifecycle / evidence-synthesis logic, over a
//! stub [`TransformRunner`] that writes a canned output dir — no real git repo,
//! no Claude credential. The decisive property: the synthesized [`EvidenceRef`]
//! round-trips through [`NameEvidenceClaims`], so an admitted local run binds
//! exactly as a wrapper-uploaded one would.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aether_bloomery::{
    Conclusion, Digest, ExecutionStatus, ExecutorBackend, Nonce, StageId, Transformation, WorkHandle,
};
use aether_bloomery_github::StageVerdict;
use tempfile::TempDir;

use super::testing::FixedRunner;
use super::{LocalExecutor, LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner};
use crate::bloomery::intake::{EvidenceClaims, NameEvidenceClaims};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

// The local backend's `Nonce` is a work-order correlation id, not a cryptographic
// nonce; deriving a test nonce off a tag here keeps the CodeQL
// hard-coded-crypto-value scan (a false positive on this correlation-id type) from
// tripping on the call sites (#3596 review).
fn test_nonce(tag: &str) -> String {
    format!("wo-{tag}")
}

fn executor(base: &TempDir, evidence: &str, lifecycle: RunLifecycle) -> LocalExecutor {
    let runner = FixedRunner { evidence: evidence.to_owned(), lifecycle };
    LocalExecutor::new(Arc::new(runner), base.path(), Some("claude-opus-4-8".to_owned()), Some("high".to_owned()))
}

fn construct_order(subject: Digest, nonce: &str) -> aether_bloomery::WorkOrder {
    aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(StageId::Construct, subject, digest(0xC0)),
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
    let exec = executor(&base, r#"{"command":"verify.check","status":"fail"}"#, RunLifecycle::Exited { success: true });

    let order = aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(StageId::Verify, subject, digest(0xC0)),
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
    let handle = exec.submit(&construct_order(digest(5), &test_nonce("g"))).unwrap();
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
    (LocalExecutor::new(Arc::new(runner), base.path(), None, None), released)
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
    // No evidence.json written → the read faults. The run stays tracked for a later
    // retry (the entry is intentionally kept), so its worktree must NOT be released
    // — releasing it would strip the checkout a retry needs.
    let base = TempDir::new().unwrap();
    let (exec, released) = recording_executor(&base, None, RunLifecycle::Exited { success: true });
    let handle = exec.submit(&construct_order(digest(5), &test_nonce("retry"))).unwrap();

    assert!(matches!(exec.stream_evidence(&handle), Err(LocalExecutorError::Evidence(_))));
    assert!(released.lock().unwrap().is_empty(), "a retryable failed read must not release the worktree");
    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Completed { conclusion: Conclusion::Success },
        "the run is retained after a failed read",
    );
}
