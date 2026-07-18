//! The local backend's registry / lifecycle / evidence-synthesis logic, over a
//! stub [`TransformRunner`] that writes a canned output dir — no real git repo,
//! no Claude credential. The decisive property: the synthesized [`EvidenceRef`]
//! round-trips through [`NameEvidenceClaims`], so an admitted local run binds
//! exactly as a wrapper-uploaded one would.

use std::sync::Arc;

use aether_bloomery::{
    Conclusion, Digest, ExecutionStatus, ExecutorBackend, Nonce, StageId, Transformation, WorkHandle,
};
use aether_bloomery_github::StageVerdict;
use tempfile::TempDir;

use super::testing::FixedRunner;
use super::{LocalExecutor, LocalExecutorError, RunLifecycle};
use crate::bloomery::intake::{EvidenceClaims, NameEvidenceClaims};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
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
