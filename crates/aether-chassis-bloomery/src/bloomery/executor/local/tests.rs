//! The local backend's registry / lifecycle / evidence-synthesis logic, over a
//! stub [`TransformRunner`] that writes a canned output dir — no real git repo,
//! no Claude credential. The decisive property: the synthesized [`EvidenceRef`]
//! round-trips through [`NameEvidenceClaims`], so an admitted local run binds
//! exactly as a wrapper-uploaded one would.

use std::collections::HashMap;
use std::fmt::{Debug, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(target_os = "linux")]
use std::process::{Child, Command};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::with_default;
use tracing::{Event, Metadata, Subscriber};

use aether_bloomery::{
    BackendObjectId, Conclusion, ConfigKind, ConfigRegistry, Correspondence, CorrespondenceError, Digest,
    ExecutionStatus, ExecutorBackend, Harness, Nonce, PriceRates, PriceTable, ReasoningEffort, ResolvedModel,
    StageCatalog, StageId, StageVerdict, Transformation, VerifyFailure, VerifyFailureSet, WorkHandle,
};
use aether_data::Kind;
use aether_data::wire::to_vec;
use tempfile::TempDir;

use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::to_hex;

#[cfg(target_os = "linux")]
use super::identity::ProcessIdentity;
#[cfg(target_os = "linux")]
use super::orphan::OrphanedRun;
use super::quarantine;
use super::runner::CapturedObjects;
use super::testing::{FixedRunner, canned_capture};
use super::{LocalExecutor, LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner};
use crate::bloomery::REQUIRED_KIT;
use crate::bloomery::executor::{OutstandingDispatch, ReconcileLanes};
use crate::bloomery::intake::{EvidenceClaims, NameEvidenceClaims};
use crate::session::{SessionKey, SessionManifest};
use crate::store::{OutstandingOrder, SqliteStore, StoreBackend};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

// A correspondence seeded with the two commits these orders carry — the
// checkout target (`for_member_stage`'s third arg, `digest(0xC0)`) and the
// sealed base its fourth names — so `submit` resolves both through it rather
// than hex-punning a digest. Both resolve in production for the same reason:
// the base is the very commit the entry-stage dispatch checked out, so an
// unresolvable one would have refused that dispatch long before a verify.
fn correspondence() -> Arc<FakeGithub> {
    let fake = FakeGithub::new();
    fake.seed_git_object(&digest(0xC0));
    fake.seed_git_object(&digest(0xB0));
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
    let runner = FixedRunner::new(evidence, lifecycle, true);
    LocalExecutor::new(Arc::new(runner), correspondence(), base.path())
}

fn construct_order(subject: Digest, nonce: &str) -> aether_bloomery::WorkOrder {
    aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Construct),
            subject,
            digest(0xC0),
            digest(0xB0),
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

/// A PATH directory whose only runnable program is `git`, so a kit-gated
/// submit sees `jscpd` (and every other row) as missing.
fn path_missing_jscpd() -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let git = dir.path().join("git");
    fs::write(&git, "#!/bin/sh\necho 'git version 2.0-test'\n").unwrap();
    let mut permissions = fs::metadata(&git).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).unwrap();
    dir
}

/// A PATH directory with a `--version`-answering stand-in for every kit tool.
fn path_with_complete_kit() -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    for tool in REQUIRED_KIT {
        let path = dir.path().join(tool.program);
        fs::write(&path, format!("#!/bin/sh\necho '{} 1.0-test'\n", tool.program)).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
    dir
}

#[test]
fn a_missing_kit_tool_refuses_submit_without_starting_the_lane() {
    // Tripwire (#5035): a host missing `jscpd` used to dispatch, burn the
    // attempt on verify.preflight, and wedge. The gate must refuse *before*
    // prepare/spawn so no outstanding order is recorded and the member stays
    // queued on the next re-drain.
    let base = TempDir::new().unwrap();
    let kit = path_missing_jscpd();
    let exec = executor(&base, r#"{"command":"construct.implement"}"#, RunLifecycle::Exited { success: true })
        .with_kit_gate(true)
        .with_kit_path(kit.path());

    let error = exec.submit(&construct_order(digest(5), "n-kit")).expect_err("a missing kit tool must refuse submit");
    match error {
        LocalExecutorError::MissingKit(refusal) => {
            assert!(refusal.contains("`jscpd`"), "the refusal names the missing tool: {refusal}");
            assert!(refusal.contains(&kit.path().display().to_string()), "and the PATH it consulted: {refusal}");
        }
        other => panic!("expected MissingKit, got {other:?}"),
    }
    assert!(
        exec.inspect(&WorkHandle::new(Nonce("n-kit".to_owned()))).unwrap() == ExecutionStatus::Unknown,
        "a refused submit must not leave a tracked run",
    );
}

#[test]
fn a_complete_kit_leaves_submit_unchanged() {
    // Acceptance: on a complete host the gate is a no-op. The stand-in PATH
    // is complete, so a regression that refuses every submit — an always-on
    // empty inspect, a list that nothing can satisfy — fails here.
    let base = TempDir::new().unwrap();
    let kit = path_with_complete_kit();
    let exec = executor(&base, r#"{"command":"construct.implement","nonce":"n-ok","produced_candidate":true,"result_record":{"schema":1,"is_error":false,"result":{"num_turns":1}}}"#, RunLifecycle::Exited { success: true })
        .with_kit_gate(true)
        .with_kit_path(kit.path());

    let handle = exec.submit(&construct_order(digest(5), "n-ok")).expect("a complete kit must dispatch");
    assert_eq!(handle, WorkHandle::new(Nonce("n-ok".to_owned())));
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
            digest(0xB0),
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
            digest(0xB0),
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
            digest(0xB0),
        ),
        nonce: Nonce("n-bad-set".to_owned()),
    };

    let reference = exec.stream_evidence(&exec.submit(&order).unwrap()).unwrap().remove(0);
    let upload = NameEvidenceClaims.claim_for(&reference).expect("the fail-closed empty-mask name decodes");

    assert_eq!(upload.verdict, StageVerdict::VerificationFailed);
    assert!(upload.failed_verifiers.is_empty(), "invalid body data is never projected as a typed claim");
}

#[test]
fn an_environment_status_yields_an_executor_fault_rather_than_a_failing_review() {
    // Tripwire (ADR-0176): the whole defect is here. While `parse_status` was
    // two-valued, an `environment` body took the `_ => None` arm, fell back to
    // the child's exit, and produced `VerificationFailed` — a verdict about a
    // candidate the critic never read. Intake then admitted a failing review and
    // the reducer re-opened members that had done nothing wrong.
    let base = TempDir::new().unwrap();
    let subject = digest(7);
    let evidence = r#"{"command":"review.critic","nonce":"n-env","status":"environment","findings":"the sandbox refused to start.\nVERDICT: environment"}"#;
    let exec = executor(&base, evidence, RunLifecycle::Exited { success: false });

    let order = aether_bloomery::WorkOrder {
        transformation: Transformation::for_aggregate_review(
            &StageCatalog::binding_of(StageId::AggregateReview),
            subject,
            digest(0xC0),
            digest(0xC0),
        ),
        nonce: Nonce("n-env".to_owned()),
    };
    let reference = exec.stream_evidence(&exec.submit(&order).unwrap()).unwrap().remove(0);
    let upload = NameEvidenceClaims.claim_for(&reference).expect("the fault name round-trips through the claim seam");

    assert_eq!(upload.verdict, StageVerdict::ExecutorFault);
    assert_eq!(upload.subject, subject, "a fault still binds the exact digest the order displayed");
    assert!(upload.failed_verifiers.is_empty(), "a fault names no verifier identity — nothing was verified");
}

#[test]
fn a_verify_lane_environment_status_is_an_unjudged_verify() {
    // Tripwire (#5089): Wave 8's dispatch-520 stamped `environment` on a
    // member Verify whose only failures sat outside the candidate's
    // reverse-dependency closure. Projecting that as `ExecutorFault` is
    // correct for AggregateReview (ADR-0176) and wrong here — intake
    // refuses a Verify executor fault and the order sits until its
    // deadline. The empty typed set is the existing unjudged-Verify
    // contract: admit, then rerun Verify mechanically.
    let base = TempDir::new().unwrap();
    let subject = digest(9);
    let evidence = r#"{"command":"verify.check","nonce":"n-verify-env","status":"environment","environment":"36 failing tests lie outside the candidate's reverse-dependency closure."}"#;
    let exec = executor(&base, evidence, RunLifecycle::Exited { success: false });

    let order = aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Verify),
            subject,
            digest(0xC0),
            digest(0xB0),
        ),
        nonce: Nonce("n-verify-env".to_owned()),
    };
    let reference = exec.stream_evidence(&exec.submit(&order).unwrap()).unwrap().remove(0);
    let upload =
        NameEvidenceClaims.claim_for(&reference).expect("the unjudged name round-trips through the claim seam");

    assert_eq!(upload.verdict, StageVerdict::VerificationFailed);
    assert_eq!(upload.subject, subject, "an unjudged verify still binds the exact digest the order displayed");
    assert!(reference.failed_verifiers.is_empty(), "the reference carries the already-decoded empty set");
    assert!(upload.failed_verifiers.is_empty(), "no verifier judged the candidate, so none is charged for it");
    assert_eq!(
        upload.detail,
        Digest::of_wire_bytes(evidence.as_bytes()),
        "the environment evidence remains the attempt detail"
    );
}

#[test]
fn an_unrecognized_or_absent_status_still_fails_closed_on_the_exit() {
    // The other half of the three-valued parse: widening the recognized set must
    // not widen what *counts*. A body claiming a status nobody stamps, or none at
    // all, falls back to the child's terminal exit exactly as before — and a
    // failed exit is a failing verdict, never a fault a bloom would retry.
    let base = TempDir::new().unwrap();
    let order = |nonce: &str| aether_bloomery::WorkOrder {
        transformation: Transformation::for_aggregate_review(
            &StageCatalog::binding_of(StageId::AggregateReview),
            digest(7),
            digest(0xC0),
            digest(0xC0),
        ),
        nonce: Nonce(nonce.to_owned()),
    };

    for (label, body) in [
        ("an invented token", r#"{"command":"review.critic","nonce":"n-odd","status":"environmental"}"#),
        ("no status at all", r#"{"command":"review.critic","nonce":"n-odd"}"#),
    ] {
        let exec = executor(&base, body, RunLifecycle::Exited { success: false });
        let reference = exec.stream_evidence(&exec.submit(&order("n-odd")).unwrap()).unwrap().remove(0);
        let upload = NameEvidenceClaims.claim_for(&reference).expect("a well-formed local name decodes");

        assert_eq!(upload.verdict, StageVerdict::VerificationFailed, "{label} must not become a host-fault claim");
    }
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
    assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Running { last_progress_unix_millis: None },);

    // A cancel kills the child and evicts the run — a subsequent inspect reports
    // the clean Unknown (never the killed child's exit as a plain completion), and
    // the registry no longer parks the terminal entry.
    exec.cancel(&handle).unwrap();
    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Unknown,
        "a cancelled run is evicted, so it reports Unknown rather than its exit"
    );
    // And the eviction does not make the next cancel a fault (ADR-0177): the
    // deadline enforcement reissues its cancel on every tick until the expired
    // order is admitted, so a refusal here would make one store fault permanent.
    exec.cancel(&handle).expect("a repeat cancel of an already-evicted run is a clean success");
}

fn evidence_dir(base: &TempDir, nonce: &str) -> PathBuf {
    base.path().join(format!("{nonce}-evidence"))
}

fn unix_millis(time: SystemTime) -> u64 {
    u64::try_from(time.duration_since(UNIX_EPOCH).expect("mtime is after the epoch").as_millis()).expect("fits u64")
}

#[test]
fn a_running_lane_reports_its_transcript_mtime_and_inspect_does_not_advance_it() {
    // The heartbeat is the transcript the worker streams, not the coordinator
    // asking. A poll that opened the file for write would make every inspect
    // look like progress and a hung harness would stay healthy forever.
    let base = TempDir::new().unwrap();
    let exec = executor(&base, "{}", RunLifecycle::Running);
    let handle = exec.submit(&construct_order(digest(5), "n-hb")).unwrap();
    let transcript = evidence_dir(&base, "n-hb").join("transcript.jsonl");
    fs::write(&transcript, b"{}\n").unwrap();
    let stamped = unix_millis(fs::metadata(&transcript).unwrap().modified().unwrap());

    let first = exec.inspect(&handle).unwrap();
    assert_eq!(first, ExecutionStatus::Running { last_progress_unix_millis: Some(stamped) });
    assert_eq!(
        unix_millis(fs::metadata(&transcript).unwrap().modified().unwrap()),
        stamped,
        "inspect must not touch the transcript it is reading",
    );
    assert_eq!(exec.inspect(&handle).unwrap(), first, "a second poll reports the same stamp, not a later one");
}

#[test]
fn an_absent_or_unreadable_or_future_transcript_is_not_fabricated_progress() {
    // Absence is "no trustworthy signal", never "silent since epoch". A
    // future stamp would extend the silence window past the sealed deadline,
    // and a broken path is the same as no file — both must stay `None`.
    let base = TempDir::new().unwrap();
    let exec = executor(&base, "{}", RunLifecycle::Running);
    let handle = exec.submit(&construct_order(digest(5), "n-none")).unwrap();
    let dir = evidence_dir(&base, "n-none");

    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Running { last_progress_unix_millis: None },
        "no transcript is not a heartbeat",
    );

    #[cfg(unix)]
    {
        symlink("/no-such-transcript", dir.join("transcript.jsonl")).unwrap();
        assert_eq!(
            exec.inspect(&handle).unwrap(),
            ExecutionStatus::Running { last_progress_unix_millis: None },
            "unreadable metadata is not a heartbeat",
        );
        fs::remove_file(dir.join("transcript.jsonl")).unwrap();
    }

    let transcript = dir.join("transcript.jsonl");
    fs::write(&transcript, b"{}\n").unwrap();
    let file = fs::File::open(&transcript).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_hours(1)).unwrap();
    drop(file);
    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Running { last_progress_unix_millis: None },
        "a future mtime is refused rather than reported as progress",
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

// A `tracing` sink that renders each event as "LEVEL field=value …" into a shared
// buffer. Local to this module because the only observable a cancel that reclaims
// nothing leaves behind is the line it logs, and asserting on that needs the
// events, not the return value.
#[derive(Default)]
struct RecordedEvents(Mutex<Vec<String>>);

impl RecordedEvents {
    fn rendered(&self) -> String {
        self.0.lock().unwrap().join("\n")
    }
}

struct EventRecorder(Arc<RecordedEvents>);

struct RenderedEvent(String);

// `%`-sigil fields arrive as `record_debug` over a `format_args!`, so one arm
// renders every field this module logs.
impl Visit for RenderedEvent {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl Subscriber for EventRecorder {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attributes: &Attributes<'_>) -> Id {
        // Nothing under test opens a span; one fixed id keeps the sink trivial.
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut rendered = RenderedEvent(event.metadata().level().to_string());
        event.record(&mut rendered);
        self.0.0.lock().unwrap().push(rendered.0);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[test]
fn cancelling_an_untracked_nonce_reports_that_it_reclaimed_nothing() {
    // Answering `Ok` for a nonce this backend never tracked is the port's
    // idempotence, not evidence of a reclaim — and the two are not
    // distinguishable from the return value. The registry is process memory that
    // boot does not rebuild, so an order dispatched before a restart takes
    // exactly this arm while its child and its scratch worktree are still live.
    // The nonce in the log is then the only thread an operator has to the orphan,
    // so the arm must not be silent.
    let base = TempDir::new().unwrap();
    let exec = executor(&base, "{}", RunLifecycle::Running);
    let events = Arc::new(RecordedEvents::default());

    with_default(EventRecorder(Arc::clone(&events)), || {
        exec.cancel(&WorkHandle::new(Nonce(test_nonce("restarted")))).unwrap();
    });

    let rendered = events.rendered();
    assert!(rendered.contains("WARN"), "a cancel that reclaimed nothing is advisory, not silent: {rendered}");
    assert!(rendered.contains("nonce=wo-restarted"), "and it names the orphan so it can be found: {rendered}");
}

#[test]
fn an_untracked_nonce_cancels_cleanly_and_streams_the_no_run_error() {
    // The two messages part company at ADR-0177: `cancel` is idempotent, so a
    // nonce with nothing running is already cancelled, while `stream_evidence`
    // still has to refuse rather than report an attempt produced no evidence.
    let base = TempDir::new().unwrap();
    let exec = executor(&base, "{}", RunLifecycle::Running);
    let ghost = WorkHandle::new(Nonce("ghost".to_owned()));

    exec.cancel(&ghost).expect("a nonce this backend never ran has nothing left to cancel");
    match exec.stream_evidence(&ghost) {
        Err(LocalExecutorError::NoRunForNonce(nonce)) => assert_eq!(nonce, Nonce("ghost".to_owned())),
        other => panic!("expected NoRunForNonce, got {other:?}"),
    }
}

// What a `RecordingRunner`-driven backend did at its spawn seam: the checkout
// each start was pointed at, in order, and every checkout it was asked to
// release. Both halves matter to the slot layout — which path a dispatch builds
// in is the whole subject of #4904, and a release is what must *not* happen to a
// slot's canonical checkout on a run's terminal path.
#[derive(Clone, Default)]
struct RunLog {
    started: Arc<Mutex<Vec<PathBuf>>>,
    // The cargo target directory each start was pointed at, in the same order —
    // the other half of where a dispatch builds (#4912), and the one a shared
    // build directory would show as a constant.
    targets: Arc<Mutex<Vec<PathBuf>>>,
    released: Arc<Mutex<Vec<PathBuf>>>,
}

impl RunLog {
    fn started(&self) -> Vec<PathBuf> {
        self.started.lock().unwrap().clone()
    }

    fn targets(&self) -> Vec<PathBuf> {
        self.targets.lock().unwrap().clone()
    }

    fn released(&self) -> Vec<PathBuf> {
        self.released.lock().unwrap().clone()
    }
}

// A spawn seam that records the checkouts it starts in and is asked to release,
// and can be told to write no `evidence.json` (so a `stream_evidence` read
// faults) — the seam for asserting which lane slot a dispatch builds in, and
// that a terminal run leaves that slot's checkout standing (#3596 review,
// resource-leak finding; #4904).
struct RecordingRunner {
    // `None` → `start` writes no evidence.json, forcing a failed `stream_evidence` read.
    evidence: Option<String>,
    lifecycle: RunLifecycle,
    log: RunLog,
    // What the backing repository reports as registered scratch checkouts — the
    // boot sweep's discriminator, which the stub otherwise has no way to have.
    registered: Vec<PathBuf>,
}

impl TransformRunner for RecordingRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        self.log.started.lock().unwrap().push(spec.worktree_dir.to_owned());
        self.log.targets.lock().unwrap().push(spec.target_dir.to_owned());
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        if let Some(evidence) = &self.evidence {
            fs::write(spec.evidence_dir.join("evidence.json"), evidence).map_err(LocalExecutorError::Io)?;
        }
        Ok(Box::new(RecordingProcess { lifecycle: self.lifecycle }))
    }

    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        self.log.released.lock().unwrap().push(worktree_dir.to_owned());
        Ok(())
    }

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError> {
        Ok(self.registered.clone())
    }

    fn capture(
        &self,
        _worktree_dir: &Path,
        _message: Option<&str>,
    ) -> Result<Option<CapturedObjects>, LocalExecutorError> {
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

fn recording_executor(base: &TempDir, evidence: Option<&str>, lifecycle: RunLifecycle) -> (LocalExecutor, RunLog) {
    let log = RunLog::default();
    let runner =
        RecordingRunner { evidence: evidence.map(str::to_owned), lifecycle, log: log.clone(), registered: Vec::new() };
    (LocalExecutor::new(Arc::new(runner), correspondence(), base.path()), log)
}

// The same seam with the repository reporting `registered` as its scratch
// checkouts — what the boot sweep reads to tell this backend's own worktrees from
// anything else living under the configured root.
fn sweeping_executor(base: &TempDir, registered: Vec<PathBuf>) -> (LocalExecutor, RunLog) {
    let log = RunLog::default();
    let runner = RecordingRunner { evidence: None, lifecycle: RunLifecycle::Running, log: log.clone(), registered };
    (LocalExecutor::new(Arc::new(runner), correspondence(), base.path()), log)
}

// The canonical checkout of lane slot `index` under a scratch root — what a
// dispatch holding that slot builds in, and the assertion target wherever a test
// says which slot a run got.
fn slot_path(base: &TempDir, index: usize) -> PathBuf {
    base.path().join(format!("slot-{index}"))
}

// The cargo target directory of lane slot `index` under a scratch root — the
// sibling of `slot_path`'s checkout, and the assertion target wherever a test
// says where a dispatch's build output goes.
fn slot_target_path(base: &TempDir, index: usize) -> PathBuf {
    base.path().join(format!("slot-{index}-target"))
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
    resume: Option<String>,
    worktree: Option<PathBuf>,
    task: Option<String>,
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
            resume: spec.resume.map(str::to_owned),
            worktree: Some(spec.worktree_dir.to_owned()),
            task: spec.task.map(str::to_owned),
        };
        Ok(Box::new(RecordingProcess { lifecycle: RunLifecycle::Running }))
    }

    fn release(&self, _worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        Ok(())
    }

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError> {
        Ok(Vec::new())
    }

    fn capture(
        &self,
        _worktree_dir: &Path,
        _message: Option<&str>,
    ) -> Result<Option<CapturedObjects>, LocalExecutorError> {
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

    // The relative base is the point of the case, so the dispatch's evidence
    // directory lands under the test process's own working directory — take it
    // back out, along with the two parents, which only go if this put them there.
    fs::remove_dir_all(&evidence_dir).unwrap();
    let _ = fs::remove_dir(".bloomery/local-worktrees");
    let _ = fs::remove_dir(".bloomery");
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

    // The mechanical verify lane is the second stage whose candidate is
    // already committed, and the range has to reach its spawn or the narrowing
    // #4890 built is inert: the lane resolves no diff base, computes no
    // closure, and recompiles the workspace on every refine lap exactly as
    // before, with nothing anywhere saying so.
    store.seed_git_object(&digest(0xB0));
    let verify = aether_bloomery::WorkOrder {
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Verify),
            digest(5),
            digest(0xC0),
            digest(0xB0),
        ),
        nonce: Nonce(test_nonce("verify")),
    };
    exec.submit(&verify).unwrap();
    assert_eq!(
        seen.lock().unwrap().diff_base.as_deref(),
        Some(to_hex(&digest(0xB0)).as_str()),
        "the verify spawn names the sealed base its candidate's diff is taken against",
    );
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
fn a_dispatch_builds_in_its_lane_slots_canonical_checkout() {
    // The whole of #4904 at the seam that decides it. `sccache` keys a
    // compilation partly by the paths cargo names on the `rustc` invocation, so
    // a dispatch that builds at a path no dispatch built at before misses on its
    // entire dependency tree — which is what a per-dispatch checkout guarantees,
    // measured as 60 hits against 268 misses on an aggregate verify. Two live
    // lanes must still hold distinct paths (they are separate working trees),
    // and the freed one must come back rather than the counter moving on: a slot
    // path handed out once and never again buys exactly nothing.
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"verify.check","nonce":"wo-a","status":"pass"}"#;
    let (exec, log) = recording_executor(&base, Some(evidence), RunLifecycle::Exited { success: true });

    let first = exec.submit(&construct_order(digest(5), &test_nonce("a"))).unwrap();
    exec.submit(&construct_order(digest(5), &test_nonce("b"))).unwrap();
    assert_eq!(log.started(), [slot_path(&base, 0), slot_path(&base, 1)], "two live lanes hold two slots");

    exec.stream_evidence(&first).unwrap();
    exec.submit(&construct_order(digest(5), &test_nonce("c"))).unwrap();

    assert_eq!(
        log.started(),
        [slot_path(&base, 0), slot_path(&base, 1), slot_path(&base, 0)],
        "the freed slot's path is where the next dispatch builds, which is what the compiler cache is keyed by",
    );
}

#[test]
fn a_dispatch_builds_into_its_lane_slots_own_target_directory() {
    // #4912, at the same seam. Two properties, and the arrangement is wrong
    // without either. The directory has to be the *slot's* — a shared one
    // serializes concurrent lanes on cargo's exclusive build lock and grows a
    // fresh artifact set per checkout path forever, a per-dispatch one is a cold
    // build every lap — and it has to sit *outside* the checkout, because the
    // dispatch that takes a slot resets it with `git clean --force --force -d
    // -x`, which removes ignored files: an in-tree target directory is deleted
    // once per dispatch, which is the same cold build with no lock contention to
    // show for it.
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"verify.check","nonce":"wo-a","status":"pass"}"#;
    let (exec, log) = recording_executor(&base, Some(evidence), RunLifecycle::Exited { success: true });

    let first = exec.submit(&construct_order(digest(5), &test_nonce("a"))).unwrap();
    exec.submit(&construct_order(digest(5), &test_nonce("b"))).unwrap();
    exec.stream_evidence(&first).unwrap();
    exec.submit(&construct_order(digest(5), &test_nonce("c"))).unwrap();

    assert_eq!(
        log.targets(),
        [slot_target_path(&base, 0), slot_target_path(&base, 1), slot_target_path(&base, 0)],
        "each slot builds into its own directory, and the dispatch that reuses a slot reuses that slot's build tree",
    );
    for (checkout, target) in log.started().iter().zip(log.targets()) {
        assert!(
            !target.starts_with(checkout),
            "{} sits inside {}, where the next dispatch's checkout hygiene deletes it",
            target.display(),
            checkout.display(),
        );
    }
}

#[test]
fn a_terminal_run_leaves_its_slots_checkout_standing() {
    // The complement, and the half a resource-leak reading would get backwards
    // (#3596 review). A slot's checkout is the slot's, not the run's: tearing it
    // down on a cancel or a consumed evidence read would re-create it from
    // scratch on the next dispatch — cold cache again — and, worse, would remove
    // a tree whichever dispatch holds that slot by then is building in. What the
    // terminal path frees is the slot; the reset the next dispatch runs is what
    // keeps the tree honest.
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"construct.implement","nonce":"wo-stream","produced_candidate":true,"result_record":{"is_error":false,"result":{}}}"#;
    let (exec, log) = recording_executor(&base, Some(evidence), RunLifecycle::Exited { success: true });

    let cancelled = exec.submit(&construct_order(digest(5), &test_nonce("cancel"))).unwrap();
    exec.cancel(&cancelled).unwrap();
    let consumed = exec.submit(&construct_order(digest(5), &test_nonce("stream"))).unwrap();
    exec.stream_evidence(&consumed).unwrap();

    assert!(log.released().is_empty(), "neither terminal path discards the slot's canonical checkout");
    assert_eq!(
        log.started(),
        [slot_path(&base, 0), slot_path(&base, 0)],
        "and the cancelled run's slot is free for the dispatch behind it",
    );
}

#[test]
fn a_failed_evidence_read_keeps_the_run_and_its_slot_for_retry() {
    // No evidence.json written yet AND the run is still Running → the missing file is
    // transient. The run stays tracked for a later retry (the entry is intentionally
    // kept), and it must keep its lane slot with it: freeing the slot would let the
    // next dispatch reset the very checkout the still-running lane is building in.
    // (An *Exited* run with no evidence is terminal instead — see
    // `an_exited_run_with_no_evidence_yields_a_failed_verdict_and_evicts`.)
    let base = TempDir::new().unwrap();
    let (exec, log) = recording_executor(&base, None, RunLifecycle::Running);
    let handle = exec.submit(&construct_order(digest(5), &test_nonce("retry"))).unwrap();

    assert!(matches!(exec.stream_evidence(&handle), Err(LocalExecutorError::Evidence(_))));
    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Running { last_progress_unix_millis: None },
        "the still-running run is retained after a transient failed read",
    );

    exec.submit(&construct_order(digest(5), &test_nonce("next"))).unwrap();
    assert_eq!(
        log.started(),
        [slot_path(&base, 0), slot_path(&base, 1)],
        "the retained run keeps slot 0, so the next dispatch builds somewhere else",
    );
}

#[test]
fn an_exited_run_with_no_evidence_yields_a_failed_verdict_and_evicts() {
    // An Exited run that left no evidence.json will never produce one — re-driving
    // the read against it loops forever (the live 2026-07-18 bloom-trial bug). It is
    // terminal: `stream_evidence` synthesizes a fail-closed VerificationFailed attempt
    // (feeding the retry/wedge machinery) and evicts the run, rather than the eternal
    // error re-drive.
    let base = TempDir::new().unwrap();
    let (exec, log) = recording_executor(&base, None, RunLifecycle::Exited { success: true });
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
    exec.submit(&construct_order(digest(5), &test_nonce("after"))).unwrap();
    assert_eq!(
        log.started(),
        [slot_path(&base, 0), slot_path(&base, 0)],
        "and its lane slot goes back, so the next dispatch builds at that path",
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
    let runner = FixedRunner::new(evidence, RunLifecycle::Exited { success: true }, true);
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

// The lane's commit message reaches both places the host uses it: the capture
// that commits under its first line, and the member row the landing assembly
// reads at the end of the bloom. Catches a message read out of the evidence and
// then dropped — the capture would silently fall back to the literal and the
// proposal to the floor, with nothing failing.
#[test]
fn a_captured_candidates_message_reaches_the_capture_and_its_member_row() {
    let base = TempDir::new().unwrap();
    let message = "feat(crate:aether-text): shelf-pack the glyph atlas\n\nGlyphs arrive one at a time.";
    let evidence = format!(
        r#"{{"command":"construct.implement","nonce":"n-msg","produced_candidate":true,"commit_message":{},"result_record":{{"schema":1,"is_error":false,"result":{{"num_turns":3}}}}}}"#,
        serde_json::to_string(message).unwrap(),
    );
    let runner = FixedRunner::new(&evidence, RunLifecycle::Exited { success: true }, true);
    let captured_messages = Arc::clone(&runner.captured_messages);
    let store = store_dir();
    let exec = LocalExecutor::new(Arc::new(runner), correspondence(), base.path())
        .with_message_store(open_store(&store, &member_order("n-msg", "issue-4242")));

    let handle = exec.submit(&construct_order(digest(5), "n-msg")).unwrap();
    assert!(exec.stream_evidence(&handle).unwrap()[0].candidate.is_some(), "the run captured a candidate");

    assert_eq!(
        captured_messages.lock().unwrap().as_slice(),
        &[Some(message.to_owned())],
        "the capture is handed the lane's own message, not the flat literal",
    );
    assert_eq!(
        SqliteStore::open(store.path().join("bloomery.sqlite").to_str().unwrap())
            .unwrap()
            .lookup_candidate_commit_message(&[1; 32], "issue-4242")
            .unwrap()
            .as_deref(),
        Some(message),
        "the message is filed against the member its order names, which is how the land path finds it",
    );
}

// The complement, and the reason the file is written on the capture rather than
// on the verdict: a run whose worktree yielded nothing leaves no row, so the next
// candidate for that member cannot inherit a message describing work that was
// never captured.
#[test]
fn a_run_that_captured_nothing_files_no_message() {
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"construct.implement","nonce":"n-void-msg","produced_candidate":true,"commit_message":"fix(crate:aether-fs): reject a traversing path","result_record":{"schema":1,"is_error":false,"result":{"num_turns":3}}}"#;
    let store = store_dir();
    let exec = LocalExecutor::new(
        Arc::new(FixedRunner::new(evidence, RunLifecycle::Exited { success: true }, false)),
        correspondence(),
        base.path(),
    )
    .with_message_store(open_store(&store, &member_order("n-void-msg", "issue-4242")));

    let handle = exec.submit(&construct_order(digest(5), "n-void-msg")).unwrap();
    assert!(exec.stream_evidence(&handle).unwrap()[0].candidate.is_none(), "nothing was captured");

    assert_eq!(
        SqliteStore::open(store.path().join("bloomery.sqlite").to_str().unwrap())
            .unwrap()
            .lookup_candidate_commit_message(&[1; 32], "issue-4242")
            .unwrap(),
        None,
        "a lost capture files no message for the candidate that never existed",
    );
}

// A file-backed store the executor and the assertion can both open — `:memory:`
// is private per connection, and `with_message_store` takes the executor's.
fn store_dir() -> TempDir {
    TempDir::new().unwrap()
}

fn open_store(dir: &TempDir, order: &OutstandingOrder) -> SqliteStore {
    let mut store = SqliteStore::open(dir.path().join("bloomery.sqlite").to_str().unwrap()).unwrap();
    store.record_order(order).unwrap();
    store
}

// An outstanding order for `nonce` naming `workpiece` in bloom `[1; 32]` — the
// (bloom, workpiece) pair the backend re-keys a captured message onto.
fn member_order(nonce: &str, workpiece: &str) -> OutstandingOrder {
    OutstandingOrder {
        nonce: nonce.to_owned(),
        bloom: vec![1; 32],
        workpiece: workpiece.to_owned(),
        scope_revision: vec![2; 32],
        candidate: vec![5; 32],
        displayed_digest: vec![5; 32],
        stage: vec![9],
        transformation: vec![7, 7],
        configs: vec![3, 3],
        profile: Vec::new(),
        deadline_unix_millis: 1_700_000_000_000,
    }
}

// ADR-0152 — fail-closed: a construct run that concluded substantively but whose
// capture found a clean worktree downgrades to a failing verdict instead of
// admitting a pass whose work was lost. Catches the inverted gate (trusting the
// child's produced_candidate stamp over the host's own capture).
#[test]
fn a_passing_construct_run_with_nothing_to_capture_fails_closed() {
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"construct.implement","nonce":"n-void","produced_candidate":true,"result_record":{"schema":1,"is_error":false,"result":{"num_turns":3}}}"#;
    let runner = FixedRunner::new(evidence, RunLifecycle::Exited { success: true }, false);
    let exec = LocalExecutor::new(Arc::new(runner), correspondence(), base.path());

    let handle = exec.submit(&construct_order(digest(5), "n-void")).unwrap();
    let refs = exec.stream_evidence(&handle).unwrap();

    assert!(refs[0].candidate.is_none());
    let upload = NameEvidenceClaims.claim_for(&refs[0]).unwrap();
    assert_eq!(upload.verdict, StageVerdict::VerificationFailed, "a lost capture is a failed attempt");
}

// A construct lane killed mid-run still captures whatever it wrote, and the
// fact that produced the capture still reads failed. Catches `passed =
// concluded || candidate.is_some()` — populating the checkpoint flipping a
// dead run into a pass.
#[test]
fn a_killed_construct_captures_its_partial_worktree_and_still_fails() {
    let base = TempDir::new().unwrap();
    let store = correspondence();
    let evidence = r#"{"command":"construct.implement","nonce":"n-kill","produced_candidate":false,"result_record":{"schema":1,"is_error":true,"result":{}}}"#;
    let runner = FixedRunner::new(evidence, RunLifecycle::Exited { success: false }, true);
    let exec = LocalExecutor::new(Arc::new(runner), Arc::clone(&store) as _, base.path());

    let handle = exec.submit(&construct_order(digest(5), "n-kill")).unwrap();
    let refs = exec.stream_evidence(&handle).unwrap();

    let candidate = refs[0].candidate.expect("a killed construct that wrote something reports its capture");
    let captured = canned_capture();
    assert_eq!(store.resolve_backend_object(&candidate.tree).unwrap().as_ref(), Some(&captured.tree));
    assert_eq!(store.resolve_backend_object(&candidate.checkout).unwrap().as_ref(), Some(&captured.commit));
    let upload = NameEvidenceClaims.claim_for(&refs[0]).unwrap();
    assert_eq!(upload.verdict, StageVerdict::VerificationFailed, "a populated candidate cannot flip passed");
    assert_eq!(upload.candidate, Some(candidate), "the claim carries the checkpoint to the intake");
}

// A killed lane that died before writing anything captures nothing and does
// not warn — an empty checkpoint is not a defect. Catches treating a clean
// death the same as a passed run's empty worktree (the fail-closed warn).
#[test]
fn a_killed_construct_with_a_clean_worktree_captures_nothing_and_does_not_warn() {
    let base = TempDir::new().unwrap();
    let evidence = r#"{"command":"construct.implement","nonce":"n-clean-die","produced_candidate":false,"result_record":{"schema":1,"no_result":true}}"#;
    let runner = FixedRunner::new(evidence, RunLifecycle::Exited { success: false }, false);
    let exec = LocalExecutor::new(Arc::new(runner), correspondence(), base.path());
    let events = Arc::new(RecordedEvents::default());

    let handle = exec.submit(&construct_order(digest(5), "n-clean-die")).unwrap();
    let refs = with_default(EventRecorder(Arc::clone(&events)), || exec.stream_evidence(&handle).unwrap());

    assert!(refs[0].candidate.is_none(), "a clean death is not a checkpoint");
    let upload = NameEvidenceClaims.claim_for(&refs[0]).unwrap();
    assert_eq!(upload.verdict, StageVerdict::VerificationFailed);
    let rendered = events.rendered();
    assert!(
        !rendered.contains("passed run left a clean worktree"),
        "a clean death must not use the passed-run fail-closed warn: {rendered}",
    );
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
    let runner = FixedRunner::new(evidence, RunLifecycle::Exited { success: true }, true);
    let exec = LocalExecutor::new(Arc::new(runner), Arc::new(RecordFaults(store)), base.path());

    let handle = exec.submit(&construct_order(digest(5), "n-fault")).unwrap();
    let refs = exec.stream_evidence(&handle).unwrap();

    assert!(refs[0].candidate.is_none(), "an unrecordable capture carries no candidate");
    let upload = NameEvidenceClaims.claim_for(&refs[0]).unwrap();
    assert_eq!(upload.verdict, StageVerdict::VerificationFailed, "an unrecordable capture is a failed attempt");
}

// The scratch root as a previous coordinator process would have left it: the
// dispatch's evidence dir, the lane slot it recorded there, the slot's checkout,
// and — when the run got that far — the `evidence.json` it wrote.
// Reconciliation reads exactly this.
fn seed_scratch(base: &TempDir, nonce: &str, slot: Option<usize>, evidence: Option<&str>) {
    let evidence_dir = base.path().join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence_dir).unwrap();
    if let Some(slot) = slot {
        fs::create_dir_all(slot_path(base, slot)).unwrap();
        fs::write(evidence_dir.join("slot"), slot.to_string()).unwrap();
    }
    if let Some(body) = evidence {
        fs::write(evidence_dir.join("evidence.json"), body).unwrap();
    }
}

fn outstanding(subject: Digest, nonce: &str) -> OutstandingDispatch {
    OutstandingDispatch {
        nonce: Nonce(nonce.to_owned()),
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Construct),
            subject,
            digest(0xC0),
            digest(0xB0),
        ),
    }
}

#[test]
fn reconcile_readopts_a_run_whose_evidence_landed_while_the_coordinator_was_down() {
    // Issue #4847: the registry is process memory, so before this the port had no
    // entry for an order a previous process dispatched — `inspect` answered
    // `Unknown` forever and the attempt never admitted, however cleanly the child
    // finished. The re-adopted run has to bind its evidence exactly as the
    // dispatching process would have: same subject axis, same lane gate.
    let base = TempDir::new().unwrap();
    let subject = digest(5);
    let nonce = test_nonce("readopt");
    let evidence = format!(
        r#"{{"command":"construct.implement","nonce":"{nonce}","produced_candidate":true,"result_record":{{"schema":1,"is_error":false,"result":{{"num_turns":3}}}}}}"#
    );
    seed_scratch(&base, &nonce, Some(0), Some(&evidence));
    let (exec, log) = recording_executor(&base, None, RunLifecycle::Running);

    let report = exec.reconcile(&[outstanding(subject, &nonce)]);

    assert_eq!(report.readopted, vec![Nonce(nonce.clone())], "the live order's surviving scratch is re-adopted");
    let handle = WorkHandle::new(Nonce(nonce.clone()));
    assert!(
        matches!(exec.inspect(&handle).unwrap(), ExecutionStatus::Completed { .. }),
        "a run whose evidence has landed reads as finished, not as an untracked Unknown",
    );

    let refs = exec.stream_evidence(&handle).unwrap();
    let upload = NameEvidenceClaims.claim_for(&refs[0]).expect("the re-adopted run's evidence decodes as an attempt");
    assert_eq!(upload.nonce, Nonce(nonce));
    assert_eq!(upload.subject, subject, "the re-adopted run binds the order's subject input, not the checkout");
    assert_eq!(
        upload.verdict,
        StageVerdict::VerificationPassed,
        "the construct gate reads the recovered body, so a substantive conclusion still passes",
    );
    assert!(log.released().is_empty(), "the slot's checkout survives the run whose evidence was consumed off it");
}

#[test]
fn a_readopted_run_whose_evidence_has_not_landed_reads_as_running() {
    // The complement, and the dangerous direction. A restart is not evidence that
    // the child died: reading an unfinished orphan as exited would send
    // `stream_evidence` down its terminal arm, synthesize a fail-closed attempt,
    // and release the worktree out from under a model lane that was still working
    // — turning every coordinator restart into destroyed in-flight work. The order
    // rides on its dispatch deadline instead, which is the mechanism for it.
    let base = TempDir::new().unwrap();
    let nonce = test_nonce("still-going");
    seed_scratch(&base, &nonce, Some(0), None);
    let (exec, log) = recording_executor(&base, None, RunLifecycle::Running);

    exec.reconcile(&[outstanding(digest(5), &nonce)]);

    assert_eq!(
        exec.inspect(&WorkHandle::new(Nonce(nonce))).unwrap(),
        ExecutionStatus::Running { last_progress_unix_millis: None },
    );
    assert!(log.released().is_empty(), "an unfinished run keeps its checkout");
}

#[test]
fn a_readopted_run_holds_the_slot_it_was_dispatched_in() {
    // The slot layout's boot hazard, and why a dispatch records its slot beside
    // its evidence. A restart is not evidence that the child died: it may still
    // be building in the slot it was dispatched into, and that slot's checkout is
    // a path this process hands out again. Re-adopt without recovering the slot
    // and the next dispatch claims it, resets the tree under a live lane, and
    // ruins both runs. Cancelling the orphan is what frees it — the child is out
    // of reach either way, but the slot is not.
    let base = TempDir::new().unwrap();
    let nonce = test_nonce("orphan");
    seed_scratch(&base, &nonce, Some(0), None);
    let (exec, log) = recording_executor(&base, None, RunLifecycle::Running);
    exec.reconcile(&[outstanding(digest(5), &nonce)]);

    exec.submit(&construct_order(digest(5), &test_nonce("fresh"))).unwrap();
    assert_eq!(log.started(), [slot_path(&base, 1)], "the orphan's slot is not handed to a fresh dispatch");

    let handle = WorkHandle::new(Nonce(nonce));
    match exec.cancel(&handle) {
        Err(LocalExecutorError::Unterminated(_)) => {}
        other => panic!("an unowned orphan must not report a kill it did not perform: {other:?}"),
    }
    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Running { last_progress_unix_millis: None },
        "an unterminated orphan stays tracked so its slot is not handed out",
    );
    assert!(
        exec.lane_occupancy().slots.contains(&0),
        "the slot it could not free is named as occupied, not hidden behind unattributed",
    );
    assert!(!exec.lane_occupancy().unattributed, "a named slot must not flip the sweep's unattributed fail-safe");
    exec.submit(&construct_order(digest(5), &test_nonce("after-cancel"))).unwrap();
    assert_eq!(
        log.started(),
        [slot_path(&base, 1), slot_path(&base, 2)],
        "the quarantined slot is not handed to the next dispatch",
    );
}

#[test]
fn a_slotless_unkillable_readopt_stays_unattributed() {
    // The fail-safe the quarantine must not weaken: a re-adopted run that
    // recorded no slot still cannot name a directory, so occupancy stays
    // unattributed and the sweep continues to evict nothing. Narrowing the
    // blanket to a named slot is the goal; dropping it for a slot-less
    // orphan is not.
    let base = TempDir::new().unwrap();
    let nonce = test_nonce("slotless-orphan");
    seed_scratch(&base, &nonce, None, None);
    let (exec, _) = recording_executor(&base, None, RunLifecycle::Running);
    exec.reconcile(&[outstanding(digest(5), &nonce)]);

    match exec.cancel(&WorkHandle::new(Nonce(nonce))) {
        Err(LocalExecutorError::Unterminated(_)) => {}
        other => panic!("an unowned orphan must not report a kill it did not perform: {other:?}"),
    }
    let occupancy = exec.lane_occupancy();
    assert!(occupancy.unattributed, "a run whose slot cannot be named still trips the sweep's fail-safe");
    assert!(occupancy.slots.is_empty(), "there is no named slot to quarantine");
}

#[test]
fn a_disk_quarantine_withholds_the_slot_from_allocation() {
    // The operator-clearable half: a quarantine file on disk, with no run in
    // this process, still keeps the slot out of `reserve_slot` and names it
    // in occupancy. Without this, a restart that does not re-adopt the order
    // would hand the checkout to a stranger while the surviving child writes.
    let base = TempDir::new().unwrap();
    quarantine::record(base.path(), 0, "wo-prior", None);
    let (exec, log) = recording_executor(&base, None, RunLifecycle::Running);

    assert!(exec.lane_occupancy().slots.contains(&0), "a disk quarantine is occupied by name");
    exec.submit(&construct_order(digest(5), &test_nonce("fresh"))).unwrap();
    assert_eq!(log.started(), [slot_path(&base, 1)], "the quarantined slot is skipped");

    quarantine::clear(base.path(), 0);
    exec.submit(&construct_order(digest(5), &test_nonce("after-clear"))).unwrap();
    assert_eq!(
        log.started(),
        [slot_path(&base, 1), slot_path(&base, 0)],
        "clearing the file returns the slot to the allocator without a coordinator restart",
    );
}

// These live-process cases observe `/proc/<pid>/stat`. Off Linux that read
// returns None, the expect panics, and the sleep child leaks into nextest.
#[cfg(target_os = "linux")]
fn spawn_isolated_sleep() -> (Child, ProcessIdentity) {
    use std::os::unix::process::CommandExt;
    let child = Command::new("sleep").arg("60").process_group(0).spawn().unwrap();
    let identity = ProcessIdentity::observe(child.id()).expect("the child is live long enough to observe");
    (child, identity)
}

#[cfg(target_os = "linux")]
#[test]
fn reattachment_refuses_a_pid_whose_start_time_does_not_match() {
    // The recycled-pid kill: a live process at the recorded pid whose start
    // time does not match is a stranger. Signalling it is strictly worse than
    // leaving the orphan alive. The refusal must leave that process running.
    let base = TempDir::new().unwrap();
    let nonce = test_nonce("mismatch");
    seed_scratch(&base, &nonce, Some(0), None);
    let (mut child, mut identity) = spawn_isolated_sleep();
    identity.starttime = identity.starttime.wrapping_add(1);
    let evidence_dir = base.path().join(format!("{nonce}-evidence"));
    identity.write(&evidence_dir).unwrap();

    let mut orphan = OrphanedRun::new(Nonce(nonce), &evidence_dir);
    match orphan.kill() {
        Err(LocalExecutorError::Unterminated(detail)) => {
            assert!(detail.contains("does not match"), "the refusal names the mismatch: {detail}");
        }
        other => panic!("a mismatched identity must not be signalled: {other:?}"),
    }
    assert!(child.try_wait().unwrap().is_none(), "the stranger at the recycled pid is still running");
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn restart_readopt_cancel_terminates_an_attached_child() {
    // The happy path this issue exists to restore: a coordinator that restarts
    // mid-dispatch re-adopts the run, re-attaches by identity, and a cancel
    // actually kills the process group rather than returning Ok for a kill it
    // never performed.
    let base = TempDir::new().unwrap();
    let nonce = test_nonce("attached");
    seed_scratch(&base, &nonce, Some(0), None);
    let (mut child, identity) = spawn_isolated_sleep();
    identity.write(&base.path().join(format!("{nonce}-evidence"))).unwrap();
    let (exec, _) = recording_executor(&base, None, RunLifecycle::Running);

    let report = exec.reconcile(&[outstanding(digest(5), &nonce)]);
    assert_eq!(report.readopted, vec![Nonce(nonce.clone())], "the live order is re-adopted");

    let handle = WorkHandle::new(Nonce(nonce));
    exec.cancel(&handle).expect("a re-attached kill reports success only after the group is gone");
    assert!(child.try_wait().unwrap().is_some(), "the re-attached cancel terminated the recorded process group");
    assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Unknown, "the terminated run is evicted");
    assert!(
        !exec.lane_occupancy().slots.contains(&0),
        "a successful kill releases the slot instead of quarantining it",
    );
}

#[test]
fn a_readopted_run_that_recorded_no_slot_captures_nothing_rather_than_guessing() {
    // The fail-safe half. A footprint from before this layout — or one whose slot
    // record never reached disk — names no checkout this process can prove belongs
    // to the run. Guessing one points the ADR-0152 capture at a tree some other
    // dispatch owns and commits *its* working state as this order's candidate. So
    // the run is re-adopted for its evidence, which is what the order is waiting
    // on, and the capture it cannot make downgrades the verdict exactly as a lost
    // capture does. (The recorded-slot case above passes through the same stub,
    // which is what makes this a discrimination rather than a stub artifact.)
    let base = TempDir::new().unwrap();
    let nonce = test_nonce("slotless");
    let evidence = format!(
        r#"{{"command":"construct.implement","nonce":"{nonce}","produced_candidate":true,"result_record":{{"schema":1,"is_error":false,"result":{{"num_turns":3}}}}}}"#
    );
    seed_scratch(&base, &nonce, None, Some(&evidence));
    let (exec, _) = recording_executor(&base, None, RunLifecycle::Running);

    let report = exec.reconcile(&[outstanding(digest(5), &nonce)]);
    assert_eq!(report.readopted, vec![Nonce(nonce.clone())], "the evidence dir is still the footprint");

    let refs = exec.stream_evidence(&WorkHandle::new(Nonce(nonce))).unwrap();
    assert!(refs[0].candidate.is_none(), "a run whose checkout cannot be named captures nothing");
    assert_eq!(
        NameEvidenceClaims.claim_for(&refs[0]).expect("the synthesized ref decodes").verdict,
        StageVerdict::VerificationFailed,
        "and the missing capture fails the attempt closed rather than admitting a stranger's tree",
    );
}

#[test]
fn reconcile_reclaims_the_scratch_of_an_order_that_is_no_longer_outstanding() {
    // The leak the sweep exists to end, and — in the same assertion — the far worse
    // inverse. The sweep's only input beyond the registrations is the live set the
    // re-adoption already read, so a checkout it removes is provably one no order is
    // waiting on; getting that backwards would delete a running lane's worktree.
    let base = TempDir::new().unwrap();
    let live = test_nonce("live");
    let consumed = test_nonce("consumed");
    // Nonce-keyed run directories are what a coordinator from before the slot
    // layout left behind, and the sweep is the only thing that ever reclaims them.
    for nonce in [&live, &consumed] {
        fs::create_dir_all(base.path().join(nonce)).unwrap();
    }
    seed_scratch(&base, &live, None, None);
    seed_scratch(&base, &consumed, None, Some("{}"));
    let registered = vec![base.path().join(&live), base.path().join(&consumed)];
    let (exec, log) = sweeping_executor(&base, registered);

    let report = exec.reconcile(&[outstanding(digest(5), &live)]);

    assert_eq!(report.reclaimed, 1, "the one abandoned checkout is reclaimed");
    assert_eq!(
        log.released(),
        vec![base.path().join(&consumed)],
        "only the abandoned checkout goes through the release seam"
    );
    assert!(!base.path().join(format!("{consumed}-evidence")).exists(), "its evidence dir goes with it");
    assert!(base.path().join(&live).exists(), "the live order's checkout is untouched");
    assert!(base.path().join(format!("{live}-evidence")).exists(), "so is its evidence dir");
}

#[test]
fn the_sweep_never_reclaims_a_lane_slots_checkout() {
    // A slot checkout is registered under the scratch root like any other, and no
    // order is ever named after it — so the sweep's "registered, and nobody is
    // waiting on it" rule would reclaim every one of them at every boot. That
    // undoes the canonical path (the next dispatch re-creates it cold) and, when
    // a re-adopted lane is still building in one, deletes a live tree. The slot's
    // checkout is bounded, reused, and reset by whoever takes the slot next; it
    // is not the sweep's to collect.
    let base = TempDir::new().unwrap();
    fs::create_dir_all(slot_path(&base, 0)).unwrap();
    let (exec, log) = sweeping_executor(&base, vec![slot_path(&base, 0)]);

    let report = exec.reconcile(&[]);

    assert_eq!(report.reclaimed, 0, "a slot checkout belongs to the slot, not to an order that has gone");
    assert!(log.released().is_empty(), "so nothing about it reaches the release seam");
    assert!(slot_path(&base, 0).exists(), "and the canonical build path is still there for the next dispatch");
}

#[test]
fn the_sweep_leaves_alone_what_this_backend_did_not_register() {
    // The scratch root is a configured path, and a deployment is entitled to keep
    // its own files under it — a scenario harness writes its lane script beside the
    // run directories. Reading the root's directory listing as "everything here is
    // mine and no order claims it" deletes those on the strength of where they sit;
    // a `git worktree` registration under the root is what the backend can actually
    // prove it created.
    let base = TempDir::new().unwrap();
    let stranger = base.path().join("not-a-run");
    fs::create_dir_all(&stranger).unwrap();
    fs::write(base.path().join("lane-script.json"), "{}").unwrap();
    let (exec, log) = sweeping_executor(&base, Vec::new());

    let report = exec.reconcile(&[]);

    assert_eq!(report.reclaimed, 0, "an unregistered path is not this backend's to reclaim");
    assert!(log.released().is_empty(), "nothing was released");
    assert!(stranger.exists(), "a directory the backend never registered survives");
    assert!(base.path().join("lane-script.json").exists(), "so does a plain file under the root");
}

#[test]
fn reconcile_never_replaces_a_run_this_process_owns() {
    // Reconciliation runs against a shared backend behind an `Arc`, so nothing
    // structurally stops a second call while runs are live. An owned run's
    // `RunProcess` is the only handle on its child; swapping it for an orphan would
    // silently retire the ability to kill that child and downgrade its lifecycle to
    // whatever the output directory happens to show. Here the owned process reports
    // a finished run while its evidence file is absent — precisely the reading an
    // orphan replacement could not produce.
    let base = TempDir::new().unwrap();
    let nonce = test_nonce("owned");
    let (exec, _released) = recording_executor(&base, None, RunLifecycle::Exited { success: true });
    let handle = exec.submit(&construct_order(digest(5), &nonce)).unwrap();

    let report = exec.reconcile(&[outstanding(digest(5), &nonce)]);

    assert!(report.readopted.is_empty(), "a tracked run is not re-adopted");
    assert_eq!(
        exec.inspect(&handle).unwrap(),
        ExecutionStatus::Completed { conclusion: Conclusion::Success },
        "the owned child's own lifecycle still answers, not one inferred from the output dir",
    );
}

// A spawn seam that records the nonce of every run it is asked to start, in
// order — what a scenario reads to see which lanes the backend has actually
// spawned, and which it is still holding behind the ceiling.
//
// Each start writes an `evidence.json` naming its own run, so a scenario can
// drain any spawned lane through `stream_evidence` (the binding is per-nonce, so
// one canned body shared across runs would refuse every one of them).
struct ThrottleRunner {
    started: Arc<Mutex<Vec<String>>>,
    // `false` → `start` writes no evidence.json, so an exited run resolves down
    // the fail-closed terminal path instead of the ordinary evidence read.
    writes_evidence: bool,
}

impl TransformRunner for ThrottleRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        self.started.lock().unwrap().push(spec.nonce.to_owned());
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        if self.writes_evidence {
            let evidence = format!(
                r#"{{"command":"construct.implement","nonce":"{}","produced_candidate":true,"result_record":{{"schema":1,"is_error":false,"result":{{"num_turns":1}}}}}}"#,
                spec.nonce
            );
            fs::write(spec.evidence_dir.join("evidence.json"), evidence).map_err(LocalExecutorError::Io)?;
        }
        Ok(Box::new(RecordingProcess { lifecycle: RunLifecycle::Exited { success: true } }))
    }

    fn release(&self, _worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        Ok(())
    }

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError> {
        Ok(Vec::new())
    }

    fn capture(
        &self,
        _worktree_dir: &Path,
        _message: Option<&str>,
    ) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        Ok(Some(canned_capture()))
    }
}

fn throttled_executor(
    base: &TempDir,
    ceiling: usize,
    writes_evidence: bool,
) -> (LocalExecutor, Arc<Mutex<Vec<String>>>) {
    let started = Arc::new(Mutex::new(Vec::new()));
    let runner = ThrottleRunner { started: Arc::clone(&started), writes_evidence };
    (LocalExecutor::new(Arc::new(runner), correspondence(), base.path()).with_max_concurrent_lanes(ceiling), started)
}

fn spawned(started: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    started.lock().unwrap().clone()
}

#[test]
fn the_lane_ceiling_holds_and_the_rest_start_in_submission_order() {
    // Each lane is a whole cargo build with its own throwaway target dir, so a
    // seal that fans out one dispatch per member must not turn member count into
    // that many simultaneous builds. Four dispatches under a ceiling of two: only
    // two children exist at a time, the other two wait, and each waiting one
    // starts — in submission order — as a running lane finishes. All four still
    // resolve, because the ceiling is a queue and never a refusal.
    let base = TempDir::new().unwrap();
    let (exec, started) = throttled_executor(&base, 2, true);

    let handles: Vec<_> = (0..4)
        .map(|index| exec.submit(&construct_order(digest(5), &test_nonce(&index.to_string()))).unwrap())
        .collect();

    assert_eq!(spawned(&started), ["wo-0", "wo-1"], "only the ceiling's worth of lanes are ever spawned at once");
    assert_eq!(exec.inspect(&handles[2]).unwrap(), ExecutionStatus::Queued, "a waiting dispatch reads as queued");

    // Draining a lane frees exactly one slot, and the queue's head takes it.
    exec.stream_evidence(&handles[0]).unwrap();
    assert_eq!(spawned(&started), ["wo-0", "wo-1", "wo-2"], "the slot goes to the dispatch that waited longest");

    exec.stream_evidence(&handles[1]).unwrap();
    assert_eq!(spawned(&started), ["wo-0", "wo-1", "wo-2", "wo-3"], "and the next slot to the one after it");

    for handle in &handles[2..] {
        let refs = exec.stream_evidence(handle).unwrap();
        assert_eq!(refs.len(), 1, "a queued dispatch resolves exactly as an immediately-started one does");
    }
}

#[test]
fn a_cancelled_dispatch_frees_its_slot_and_a_cancelled_queued_one_never_starts() {
    // Both halves of a cancel under the ceiling. A cancelled *running* lane must
    // release its slot, or a bloom whose lanes are cancelled at their deadline
    // wedges the queue behind them. A cancelled *waiting* one must leave the
    // queue: the reactor cancels an order it has already expired, and starting a
    // lane for it afterwards spends a slot — and a whole cargo build — on work
    // nothing will admit.
    let base = TempDir::new().unwrap();
    let (exec, started) = throttled_executor(&base, 1, true);

    let running = exec.submit(&construct_order(digest(5), &test_nonce("running"))).unwrap();
    let expired = exec.submit(&construct_order(digest(5), &test_nonce("expired"))).unwrap();
    let next = exec.submit(&construct_order(digest(5), &test_nonce("next"))).unwrap();
    assert_eq!(spawned(&started), ["wo-running"], "the ceiling of one admits one child");

    exec.cancel(&expired).unwrap();
    assert_eq!(exec.inspect(&expired).unwrap(), ExecutionStatus::Unknown, "a cancelled dispatch is no longer queued");

    exec.cancel(&running).unwrap();
    assert_eq!(
        spawned(&started),
        ["wo-running", "wo-next"],
        "the freed slot skips the cancelled dispatch and starts the one behind it",
    );
    assert_eq!(
        exec.inspect(&next).unwrap(),
        ExecutionStatus::Completed { conclusion: Conclusion::Success },
        "the dispatch that took the slot is a tracked run like any other",
    );
}

#[test]
fn a_run_that_fails_without_evidence_frees_its_slot() {
    // The fail-closed terminal path — an exited run that left no readable
    // evidence — evicts the run on its own, so it has to free the lane slot as
    // well. Missing that, a bloom whose lanes all die this way holds every slot
    // it ever took and the queue behind them never moves.
    let base = TempDir::new().unwrap();
    let (exec, started) = throttled_executor(&base, 1, false);

    let failing = exec.submit(&construct_order(digest(5), &test_nonce("failing"))).unwrap();
    let waiting = exec.submit(&construct_order(digest(5), &test_nonce("waiting"))).unwrap();
    assert_eq!(spawned(&started), ["wo-failing"]);

    let refs = exec.stream_evidence(&failing).unwrap();
    assert_eq!(refs.len(), 1, "the failed run still synthesizes its fail-closed attempt");
    assert_eq!(spawned(&started), ["wo-failing", "wo-waiting"], "and releases its slot to the queue");
    assert_ne!(exec.inspect(&waiting).unwrap(), ExecutionStatus::Queued, "the waiting dispatch is now a live run");
}

fn claude_order(subject: Digest, nonce: &str, task: &str) -> aether_bloomery::WorkOrder {
    let mut order = construct_order(subject, nonce);
    order.transformation.model = Some(ResolvedModel {
        harness: Harness::Claude,
        model: "claude-opus-5".to_owned(),
        effort: ReasoningEffort::High,
    });
    order.transformation.description = Some(task.to_owned());
    order
}

fn grok_order(subject: Digest, nonce: &str, task: &str) -> aether_bloomery::WorkOrder {
    let mut order = claude_order(subject, nonce, task);
    order.transformation.model =
        Some(ResolvedModel { harness: Harness::Grok, model: "grok-4.6".to_owned(), effort: ReasoningEffort::High });
    order
}

fn reuse_evidence(nonce: &str, session_id: &str, input_tokens: u64) -> String {
    format!(
        r#"{{"command":"construct.implement","nonce":"{nonce}","produced_candidate":true,"result_record":{{"schema":1,"is_error":false,"session_id":"{session_id}","input":{input_tokens},"cache_read":0,"cache_write":4000,"output":200,"num_turns":3,"result":{{"num_turns":3,"session_id":"{session_id}"}}}}}}"#
    )
}

/// A stub that writes nonce-tagged evidence and records every `RunSpec`, so a
/// two-lap fixture can deposit from lap 1 and see lap 2's resume handle.
struct ReuseRunner {
    seen: Arc<Mutex<Vec<SeenSpec>>>,
    /// Uncached input tokens stamped on the result record. The default keeps
    /// T small so the seed still resumes; a large value is what lets a
    /// sealed even-rate table flip lap 2 cold.
    input_tokens: u64,
    /// A nonce that stays `Running` so its slot stays held — the fixture for
    /// forcing a predecessor into a non-lowest slot.
    hold: Option<String>,
    /// Direct parents `checkout_parents` reports for a checkout hex.
    parents: HashMap<String, Vec<String>>,
    /// When set, `checkout_parents` fails rather than returning a list.
    fail_parents: bool,
}

impl ReuseRunner {
    fn new(seen: Arc<Mutex<Vec<SeenSpec>>>) -> Self {
        Self { seen, input_tokens: 1_000, hold: None, parents: HashMap::new(), fail_parents: false }
    }

    fn with_input_tokens(mut self, input_tokens: u64) -> Self {
        self.input_tokens = input_tokens;
        self
    }

    fn holding(mut self, nonce: impl Into<String>) -> Self {
        self.hold = Some(nonce.into());
        self
    }

    fn with_parents(mut self, checkout: impl Into<String>, parents: Vec<String>) -> Self {
        self.parents.insert(checkout.into(), parents);
        self
    }

    fn failing_parents(mut self) -> Self {
        self.fail_parents = true;
        self
    }
}

impl TransformRunner for ReuseRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        self.seen.lock().unwrap().push(SeenSpec {
            evidence_dir: Some(spec.evidence_dir.to_owned()),
            harness: spec.harness.map(str::to_owned),
            model: spec.model.map(str::to_owned),
            effort: spec.effort.map(str::to_owned),
            checkout: Some(spec.checkout_hex.to_owned()),
            diff_base: spec.diff_base_hex.map(str::to_owned),
            resume: spec.resume.map(str::to_owned),
            worktree: Some(spec.worktree_dir.to_owned()),
            task: spec.task.map(str::to_owned),
        });
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        fs::write(spec.evidence_dir.join("evidence.json"), reuse_evidence(spec.nonce, "sess-1", self.input_tokens))
            .map_err(LocalExecutorError::Io)?;
        let lifecycle = if self.hold.as_deref() == Some(spec.nonce) {
            RunLifecycle::Running
        } else {
            RunLifecycle::Exited { success: true }
        };
        Ok(Box::new(RecordingProcess { lifecycle }))
    }

    fn release(&self, _worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        Ok(())
    }

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError> {
        Ok(Vec::new())
    }

    fn capture(
        &self,
        _worktree_dir: &Path,
        _message: Option<&str>,
    ) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        Ok(Some(canned_capture()))
    }

    fn checkout_parents(&self, checkout_hex: &str) -> Result<Vec<String>, LocalExecutorError> {
        if self.fail_parents {
            return Err(LocalExecutorError::Worktree("checkout parents unreadable".to_owned()));
        }
        Ok(self.parents.get(checkout_hex).cloned().unwrap_or_default())
    }
}

#[test]
fn a_second_lap_resumes_the_deposited_session() {
    // Acceptance: lap 2 acquires the session lap 1 deposited, and the spawn
    // carries the resume handle. A miss here is the whole cost the pool exists
    // to avoid — a cold relaunch of the exploration prefix.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    let exec = LocalExecutor::new(Arc::new(ReuseRunner::new(Arc::clone(&seen))), correspondence(), base.path())
        .with_session_reuse(sessions);

    let first = exec.submit(&claude_order(digest(5), &test_nonce("lap-1"), "issue-4902")).unwrap();
    exec.stream_evidence(&first).unwrap();
    assert!(seen.lock().unwrap()[0].resume.is_none(), "lap 1 launches cold");

    let second = exec.submit(&claude_order(digest(5), &test_nonce("lap-2"), "issue-4902")).unwrap();
    let refs = exec.stream_evidence(&second).unwrap();
    assert_eq!(seen.lock().unwrap()[1].resume.as_deref(), Some("sess-1"), "lap 2 threads the deposited session");

    let stamped: serde_json::Value =
        serde_json::from_slice(&fs::read(base.path().join("wo-lap-2-evidence/evidence.json")).unwrap()).unwrap();
    assert_eq!(stamped["session_reuse"]["arm"], "resumed");
    assert_eq!(stamped["session_reuse"]["predicted_arm"], "resumed");
    assert_eq!(stamped["session_reuse"]["actual_turns"], 3);
    assert_eq!(refs[0].nonce, Nonce(test_nonce("lap-2")));
}

#[test]
fn a_named_miss_falls_back_to_fresh_and_stamps_the_reason() {
    // Each stated miss reason must be visible in the journaled evidence — the
    // reuse rate is otherwise unauditable. One fixture per reason would repeat
    // the same predicate; the pool names them, and this stamps the host path.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    sessions.seed(
        &SessionKey {
            model: "claude-opus-5".to_owned(),
            effort: "high".to_owned(),
            task: super::session_reuse::pool_task("construct.implement", Some("issue-4902")),
        },
        "sess-1",
        &SessionManifest {
            parent_receipt: None,
            receipt: "receipt".to_owned(),
            head_hash: "head-A".to_owned(),
            context_tokens: 8_000,
            workspace_tree_hash: String::new(),
            read_files: Vec::new(),
            deposited_at: 1_000,
        },
        "/other-slot",
    );
    let exec = LocalExecutor::new(Arc::new(ReuseRunner::new(Arc::clone(&seen))), correspondence(), base.path())
        .with_session_reuse(sessions);

    let handle = exec.submit(&claude_order(digest(5), &test_nonce("miss"), "issue-4902")).unwrap();
    exec.stream_evidence(&handle).unwrap();

    assert!(seen.lock().unwrap()[0].resume.is_none(), "a slot mismatch launches fresh");
    let stamped: serde_json::Value =
        serde_json::from_slice(&fs::read(base.path().join("wo-miss-evidence/evidence.json")).unwrap()).unwrap();
    assert_eq!(stamped["session_reuse"]["arm"], "fresh");
    assert_eq!(stamped["session_reuse"]["miss"], "slot_mismatch");
}

#[test]
fn a_second_grok_lap_resumes_the_deposited_session() {
    // Grok holds the volume seats, so the executor must hand a grok lap the
    // same deposited session it hands a claude lap. The bug this catches is a
    // harness-conditional acquire on the host path: the pool leases the row,
    // and the spawn drops the handle for anything that is not claude — the
    // stamped arm would say "resumed" while the process relaunched cold.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    let exec = LocalExecutor::new(Arc::new(ReuseRunner::new(Arc::clone(&seen))), correspondence(), base.path())
        .with_session_reuse(sessions);

    let first = exec.submit(&grok_order(digest(5), &test_nonce("grok-1"), "issue-4902")).unwrap();
    exec.stream_evidence(&first).unwrap();
    assert!(seen.lock().unwrap()[0].resume.is_none(), "lap 1 launches cold");

    let second = exec.submit(&grok_order(digest(5), &test_nonce("grok-2"), "issue-4902")).unwrap();
    exec.stream_evidence(&second).unwrap();
    assert_eq!(seen.lock().unwrap()[1].resume.as_deref(), Some("sess-1"), "lap 2 threads the deposited session");

    let stamped: serde_json::Value =
        serde_json::from_slice(&fs::read(base.path().join("wo-grok-2-evidence/evidence.json")).unwrap()).unwrap();
    assert_eq!(stamped["session_reuse"]["arm"], "resumed");
    assert_eq!(stamped["session_reuse"]["miss"], serde_json::Value::Null, "no harness-named miss remains");
}

#[test]
fn a_critic_does_not_resume_the_constructors_session() {
    // The pool key used to be (model, effort, description). A review.critic
    // dispatch that shares the construct lap's resolved profile and work-order
    // text would then resume the constructor and judge the candidate carrying
    // the implementer's context.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    let exec = LocalExecutor::new(Arc::new(ReuseRunner::new(Arc::clone(&seen))), correspondence(), base.path())
        .with_session_reuse(sessions);

    let first = exec.submit(&claude_order(digest(5), &test_nonce("construct"), "issue-4902")).unwrap();
    exec.stream_evidence(&first).unwrap();

    let mut critic = aether_bloomery::WorkOrder {
        transformation: Transformation::for_aggregate_review(
            &StageCatalog::binding_of(StageId::AggregateReview),
            digest(5),
            digest(0xC0),
            digest(0xC0),
        ),
        nonce: Nonce(test_nonce("critic")),
    };
    critic.transformation.model = Some(ResolvedModel {
        harness: Harness::Claude,
        model: "claude-opus-5".to_owned(),
        effort: ReasoningEffort::High,
    });
    critic.transformation.description = Some("issue-4902".to_owned());
    let handle = exec.submit(&critic).unwrap();
    exec.stream_evidence(&handle).unwrap();

    assert!(seen.lock().unwrap()[1].resume.is_none(), "the critic must not inherit the constructor's session");
    let stamped: serde_json::Value =
        serde_json::from_slice(&fs::read(base.path().join("wo-critic-evidence/evidence.json")).unwrap()).unwrap();
    assert_eq!(stamped["session_reuse"]["arm"], "fresh");
}

#[test]
fn a_sealed_price_table_decides_the_lap_two_arm() {
    // Tripwire: production used to build SessionReuse over PriceTable::default(),
    // so the acquire inequality never saw the bloom's sealed rates and lap 2
    // always resumed. A table that makes resume dearer than cold must flip
    // the predicted arm.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();
    let store_path = store_dir.path().join("store.db");
    let mut store = SqliteStore::open(store_path.to_str().unwrap()).unwrap();

    let mut table = PriceTable::default();
    table.rows.insert(
        "claude-opus-5".to_owned(),
        PriceRates {
            input: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            cache_write: 1_000_000,
            output: 1_000_000,
            long_context: None,
        },
    );
    let bytes = to_vec(&table).unwrap();
    store.record_config(table.address().as_bytes(), PriceTable::NAME, &bytes).unwrap();
    let mut configs = ConfigRegistry::default();
    configs.insert::<PriceTable>(table.address());
    let configs = to_vec(&configs).unwrap();
    for nonce in [test_nonce("priced-1"), test_nonce("priced-2")] {
        store
            .record_order(&OutstandingOrder {
                nonce,
                bloom: vec![1; 32],
                workpiece: "issue-4902".to_owned(),
                scope_revision: vec![2; 32],
                candidate: vec![5; 32],
                displayed_digest: vec![5; 32],
                stage: vec![9],
                transformation: vec![7, 7],
                configs: configs.clone(),
                profile: Vec::new(),
                deadline_unix_millis: 1_700_000_000_000,
            })
            .unwrap();
    }

    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    let exec = LocalExecutor::new(
        Arc::new(ReuseRunner::new(Arc::clone(&seen)).with_input_tokens(80_000)),
        correspondence(),
        base.path(),
    )
    .with_session_reuse(sessions)
    .with_message_store(store);

    let first = exec.submit(&claude_order(digest(5), &test_nonce("priced-1"), "issue-4902")).unwrap();
    exec.stream_evidence(&first).unwrap();
    let second = exec.submit(&claude_order(digest(5), &test_nonce("priced-2"), "issue-4902")).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped: serde_json::Value =
        serde_json::from_slice(&fs::read(base.path().join("wo-priced-2-evidence/evidence.json")).unwrap()).unwrap();
    assert_eq!(
        stamped["session_reuse"]["predicted_arm"], "fresh",
        "even sealed rates against a large prior T go cold; an empty table would have resumed"
    );
    assert!(seen.lock().unwrap()[1].resume.is_none());
}

fn stamped_evidence(base: &TempDir, nonce: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(base.path().join(format!("{nonce}-evidence/evidence.json"))).unwrap())
        .expect("stamped evidence is JSON")
}

fn order_on(mut order: aether_bloomery::WorkOrder, checkout: Digest) -> aether_bloomery::WorkOrder {
    order.transformation.checkout = checkout;
    order
}

#[test]
fn a_dependent_construct_prefers_the_predecessors_slot() {
    // Acceptance: B lands in A's slot when free. Lowest-free would take 0 once
    // the holder is gone; preferring 1 is the only way B hits the warm target
    // dir A already compiled.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let holder = test_nonce("hold");
    let exec = LocalExecutor::new(
        Arc::new(ReuseRunner::new(Arc::clone(&seen)).holding(holder.clone())),
        correspondence(),
        base.path(),
    );

    let holding = exec.submit(&construct_order(digest(5), &holder)).unwrap();
    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    let refs = exec.stream_evidence(&first).unwrap();
    let checkout = refs[0].candidate.expect("A captured").checkout;
    exec.cancel(&holding).unwrap();

    let second = exec.submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), checkout)).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert_eq!(stamped["slot_affinity"]["preferred"], 1);
    assert_eq!(stamped["slot_affinity"]["assigned"], 1);
    assert_eq!(stamped["slot_affinity"]["reason"], "preferred");
    assert_eq!(
        seen.lock().unwrap().last().and_then(|seen| seen.worktree.clone()),
        Some(slot_path(&base, 1)),
        "B built in A's slot, not the newly-freed lowest index"
    );
}

#[test]
fn a_dependent_construct_falls_back_when_the_preferred_slot_is_busy() {
    // Acceptance: preference is never a wait. A still occupies slot 1; B
    // takes the lowest free (0) and journals `busy`.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let holder = test_nonce("hold");
    let exec = LocalExecutor::new(
        Arc::new(ReuseRunner::new(Arc::clone(&seen)).holding(holder.clone())),
        correspondence(),
        base.path(),
    );

    let _holding = exec.submit(&construct_order(digest(5), &holder)).unwrap();
    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    let refs = exec.stream_evidence(&first).unwrap();
    let checkout = refs[0].candidate.expect("A captured").checkout;

    // Re-occupy A's slot so B cannot have it.
    let occupier = test_nonce("occupy");
    let occupying = exec.submit(&construct_order(digest(5), &occupier)).unwrap();
    // occupier exits immediately (not held), so slot 1 is free again unless we
    // don't stream it and... it already exited, retire happens on stream, not
    // on exit. Slot stays held until stream_evidence or cancel.
    let _ = occupying;

    let second = exec.submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), checkout)).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert_eq!(stamped["slot_affinity"]["preferred"], 1);
    assert_eq!(stamped["slot_affinity"]["reason"], "busy");
    assert_ne!(stamped["slot_affinity"]["assigned"], 1, "B must not wait on A's occupied slot");
}

fn canned_commit_hex() -> String {
    "cc".repeat(20)
}

fn fold_digest() -> Digest {
    digest(0xF0)
}

fn fold_hex() -> String {
    to_hex(&fold_digest())
}

fn integration_hex() -> String {
    "11".repeat(20)
}

fn decoy_hex() -> String {
    "dd".repeat(20)
}

fn correspondence_with_fold() -> Arc<FakeGithub> {
    let fake = FakeGithub::new();
    fake.seed_git_object(&digest(0xC0));
    fake.seed_git_object(&digest(0xB0));
    fake.seed_git_object(&fold_digest());
    Arc::new(fake)
}

fn assigned_slot(base: &TempDir, nonce: &str) -> usize {
    fs::read_to_string(base.path().join(format!("{nonce}-evidence/slot")))
        .expect("every dispatched run records its slot")
        .trim()
        .parse()
        .expect("the slot record is an index")
}

fn fold_parents(integration: String, candidate: String) -> (String, Vec<String>) {
    (fold_hex(), vec![integration, candidate])
}

#[test]
fn a_fold_checkout_prefers_the_captured_candidates_slot() {
    // Acceptance (#5077): B's checkout is the synthetic fold
    // `fold(integration, captured candidate)`, not the candidate itself.
    // Exact-key affinity misses; the direct candidate parent must still
    // land B in A's slot even though slot 0 is free.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let holder = test_nonce("hold");
    let (fold, parents) = fold_parents(integration_hex(), canned_commit_hex());
    let exec = LocalExecutor::new(
        Arc::new(ReuseRunner::new(Arc::clone(&seen)).holding(holder.clone()).with_parents(fold, parents)),
        correspondence_with_fold(),
        base.path(),
    );

    let holding = exec.submit(&construct_order(digest(5), &holder)).unwrap();
    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    exec.stream_evidence(&first).unwrap();
    exec.cancel(&holding).unwrap();

    let second = exec.submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), fold_digest())).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert_eq!(stamped["slot_affinity"]["preferred"], 1);
    assert_eq!(stamped["slot_affinity"]["assigned"], 1);
    assert_eq!(stamped["slot_affinity"]["reason"], "preferred");
    assert_eq!(
        seen.lock().unwrap().last().and_then(|seen| seen.worktree.clone()),
        Some(slot_path(&base, 1)),
        "B built in A's slot, not the newly-freed lowest index",
    );
}

#[test]
fn a_fold_checkout_falls_back_when_the_preferred_slot_is_busy() {
    // Preference is never a wait, including across a fold parent. A still
    // occupies slot 1; B takes the lowest free and journals `busy`.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let holder = test_nonce("hold");
    let (fold, parents) = fold_parents(integration_hex(), canned_commit_hex());
    let exec = LocalExecutor::new(
        Arc::new(ReuseRunner::new(Arc::clone(&seen)).holding(holder.clone()).with_parents(fold, parents)),
        correspondence_with_fold(),
        base.path(),
    );

    let _holding = exec.submit(&construct_order(digest(5), &holder)).unwrap();
    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    exec.stream_evidence(&first).unwrap();
    let occupying = exec.submit(&construct_order(digest(5), &test_nonce("occupy"))).unwrap();
    let _ = occupying;

    let second = exec.submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), fold_digest())).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert_eq!(stamped["slot_affinity"]["preferred"], 1);
    assert_eq!(stamped["slot_affinity"]["reason"], "busy");
    assert_ne!(stamped["slot_affinity"]["assigned"], 1, "B must not wait on A's occupied slot");
}

#[test]
fn a_mapped_ancestor_that_is_not_a_direct_parent_is_ignored() {
    // Walking past the fold's direct parents would prefer a stale historical
    // builder. Integration's parent is the captured candidate, but B must
    // not consult it.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let holder = test_nonce("hold");
    let exec = LocalExecutor::new(
        Arc::new(
            ReuseRunner::new(Arc::clone(&seen))
                .holding(holder.clone())
                .with_parents(fold_hex(), vec![integration_hex(), decoy_hex()])
                .with_parents(integration_hex(), vec![canned_commit_hex()]),
        ),
        correspondence_with_fold(),
        base.path(),
    );

    let holding = exec.submit(&construct_order(digest(5), &holder)).unwrap();
    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    exec.stream_evidence(&first).unwrap();
    exec.cancel(&holding).unwrap();

    let second = exec.submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), fold_digest())).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert!(stamped.get("slot_affinity").is_none(), "no predecessor slot: the mapped ancestor is not a direct parent");
    assert_eq!(assigned_slot(&base, &test_nonce("B")), 0, "B takes the lowest free slot");
}

#[test]
fn an_exact_checkout_match_wins_over_a_mapped_parent() {
    // Exact-key affinity is still first. The checkout itself is mapped to
    // A's slot; a parent mapped to another slot must not steal the choice.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    fs::write(base.path().join("edge-slots"), format!("{} 2\n", decoy_hex())).unwrap();
    let holder = test_nonce("hold");
    let exec = LocalExecutor::new(
        Arc::new(
            ReuseRunner::new(Arc::clone(&seen))
                .holding(holder.clone())
                .with_parents(canned_commit_hex(), vec![decoy_hex()]),
        ),
        correspondence(),
        base.path(),
    );

    let holding = exec.submit(&construct_order(digest(5), &holder)).unwrap();
    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    let refs = exec.stream_evidence(&first).unwrap();
    let checkout = refs[0].candidate.expect("A captured").checkout;
    exec.cancel(&holding).unwrap();

    let second = exec.submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), checkout)).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert_eq!(stamped["slot_affinity"]["preferred"], 1);
    assert_eq!(stamped["slot_affinity"]["assigned"], 1);
    assert_eq!(stamped["slot_affinity"]["reason"], "preferred");
}

#[test]
fn an_unreadable_fold_parent_list_falls_back_without_refusing_the_order() {
    // Parent inspection is diagnostic. A git miss must not refuse B; it
    // degrades to the ordinary lowest-free allocator.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let holder = test_nonce("hold");
    let exec = LocalExecutor::new(
        Arc::new(ReuseRunner::new(Arc::clone(&seen)).holding(holder.clone()).failing_parents()),
        correspondence_with_fold(),
        base.path(),
    );

    let holding = exec.submit(&construct_order(digest(5), &holder)).unwrap();
    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    exec.stream_evidence(&first).unwrap();
    exec.cancel(&holding).unwrap();

    let second = exec
        .submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), fold_digest()))
        .expect("an unreadable parent list must not refuse dispatch");
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert!(stamped.get("slot_affinity").is_none(), "fallback is no preference, not a refused order");
    assert_eq!(assigned_slot(&base, &test_nonce("B")), 0);
}

#[test]
fn a_cross_seat_dependent_never_resumes_the_predecessors_session() {
    // Acceptance: grok built A; claude on B, even in A's slot, launches fresh.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    let holder = test_nonce("hold");
    let exec = LocalExecutor::new(
        Arc::new(ReuseRunner::new(Arc::clone(&seen)).holding(holder.clone())),
        correspondence(),
        base.path(),
    )
    .with_session_reuse(sessions);

    let holding = exec.submit(&construct_order(digest(5), &holder)).unwrap();
    let first = exec.submit(&grok_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    let refs = exec.stream_evidence(&first).unwrap();
    let checkout = refs[0].candidate.expect("A captured").checkout;
    exec.cancel(&holding).unwrap();

    let second = exec.submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), checkout)).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert_eq!(stamped["slot_affinity"]["reason"], "preferred", "B still prefers A's slot");
    assert_eq!(stamped["session_reuse"]["arm"], "fresh");
    assert_eq!(stamped["session_reuse"]["edge"], false);
    assert!(seen.lock().unwrap().last().unwrap().resume.is_none());
}

#[test]
fn a_judge_dispatch_never_acquires_a_builder_session() {
    // Acceptance: Review / AggregateReview seats never resume a builder. The
    // new slot-path handle would leak the constructor's session into the
    // critic if the judge path consulted it.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    let exec = LocalExecutor::new(Arc::new(ReuseRunner::new(Arc::clone(&seen))), correspondence(), base.path())
        .with_session_reuse(sessions);

    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    exec.stream_evidence(&first).unwrap();

    let mut critic = aether_bloomery::WorkOrder {
        transformation: Transformation::for_aggregate_review(
            &StageCatalog::binding_of(StageId::AggregateReview),
            digest(5),
            digest(0xC0),
            digest(0xC0),
        ),
        nonce: Nonce(test_nonce("judge")),
    };
    critic.transformation.model = Some(ResolvedModel {
        harness: Harness::Claude,
        model: "claude-opus-5".to_owned(),
        effort: ReasoningEffort::High,
    });
    critic.transformation.description = Some("issue-A".to_owned());
    let handle = exec.submit(&critic).unwrap();
    exec.stream_evidence(&handle).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("judge"));
    assert_eq!(stamped["session_reuse"]["arm"], "fresh");
    assert_eq!(stamped["session_reuse"]["edge"], false);
    assert!(seen.lock().unwrap()[1].resume.is_none(), "the judge must not inherit the builder's session");
}

#[test]
fn dispatch_evidence_carries_fresh_or_resumed_and_token_figures() {
    // Acceptance: every dispatch journals the arm plus sealed-table token
    // figures. A missing priced column is the harness's dollar figure sneaking
    // back in — the study path already refuses that.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    let exec = LocalExecutor::new(Arc::new(ReuseRunner::new(Arc::clone(&seen))), correspondence(), base.path())
        .with_session_reuse(sessions);

    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    exec.stream_evidence(&first).unwrap();
    let fresh = stamped_evidence(&base, &test_nonce("A"));
    assert_eq!(fresh["session_reuse"]["arm"], "fresh");
    assert_eq!(fresh["session_reuse"]["input_tokens"], 1_000);
    assert_eq!(fresh["session_reuse"]["cache_write_tokens"], 4_000);
    assert_eq!(fresh["session_reuse"]["output_tokens"], 200);
    assert_eq!(fresh["session_reuse"]["actual_turns"], 3);

    let second = exec.submit(&claude_order(digest(5), &test_nonce("A2"), "issue-A")).unwrap();
    exec.stream_evidence(&second).unwrap();
    let resumed = stamped_evidence(&base, &test_nonce("A2"));
    assert_eq!(resumed["session_reuse"]["arm"], "resumed");
    assert_eq!(resumed["session_reuse"]["input_tokens"], 1_000);
}

#[test]
fn a_dependent_construct_resumes_the_predecessors_session_and_is_told_about_the_splice() {
    // Host path for the edge: B's first lap is cold on its own key, so without
    // the predecessor lookup it would relaunch. The resumed prompt must name
    // the splice — the reset statement the pool already adds is about files
    // this session edited, which is the wrong fact along an edge.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = TempDir::new().unwrap();
    let sessions = super::SessionReuse::memory(PriceTable::default());
    sessions.set_head_hash("head-A");
    sessions.set_now(1_000);
    let exec = LocalExecutor::new(Arc::new(ReuseRunner::new(Arc::clone(&seen))), correspondence(), base.path())
        .with_session_reuse(sessions);

    let first = exec.submit(&claude_order(digest(5), &test_nonce("A"), "issue-A")).unwrap();
    let refs = exec.stream_evidence(&first).unwrap();
    let checkout = refs[0].candidate.expect("A captured").checkout;

    let second = exec.submit(&order_on(claude_order(digest(5), &test_nonce("B"), "issue-B"), checkout)).unwrap();
    exec.stream_evidence(&second).unwrap();

    let stamped = stamped_evidence(&base, &test_nonce("B"));
    assert_eq!(stamped["session_reuse"]["arm"], "resumed");
    assert_eq!(stamped["session_reuse"]["edge"], true);
    assert_eq!(seen.lock().unwrap()[1].resume.as_deref(), Some("sess-1"));
    assert!(
        seen.lock().unwrap()[1].task.as_deref().is_some_and(|task| task.contains("spliced dependency candidate")),
        "the resumed prompt must state what was spliced, not only that the tree was reset"
    );
}
