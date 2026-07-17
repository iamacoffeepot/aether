//! Evidence-intake tests (#3502): the broker accept-gate (nonce + digest match,
//! consume-once), the dispatch-record write, and the pull-loop cycle driven
//! end-to-end with the reducer as the oracle.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use aether_bloomery::{
    BloomDraft, BloomId, BloomRecord, BloomStatus, Budget, Digest, Evidence, EvidenceKind, EvidenceRef, Fact, Forecast,
    Membership, NetworkProfile, Nonce, Outcome, Snapshot, Transformation, WorkHandle, WorkOrder, WorkpieceId, reduce,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{ActionsExecutor, Artifact, RunConclusion, RunStatus, StageVerdict};

use super::{
    Admission, AdmitDecision, AdmitSink, DispatchRecord, EvidenceClaims, IntakeRefusal, UploadedEvidence,
    admit_uploaded, dispatch_and_record, record_dispatch, run_intake_cycle,
};
use crate::bloomery::ExecutorShell;
use crate::store::{SqliteStore, StoreBackend};

const WORKFLOW: &str = "bloomery-transform.yml";
const PINNED_REF: &str = "refs/heads/main";

fn store() -> SqliteStore {
    SqliteStore::open(":memory:").unwrap()
}

fn shell(fake: FakeGithub) -> ExecutorShell {
    ExecutorShell::new(Arc::new(ActionsExecutor::new(fake, WORKFLOW, PINNED_REF)))
}

fn work_order(nonce: &str) -> WorkOrder {
    WorkOrder {
        transformation: Transformation {
            command: "verify.clippy".to_owned(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            image: "iama/verify:1".to_owned(),
            limits: Budget::default(),
            network: NetworkProfile::None,
        },
        nonce: Nonce(nonce.to_owned()),
    }
}

// A record whose `candidate == displayed_digest` (a well-formed order): the
// digest Bloomery displayed is the candidate the evidence must bind to.
fn dispatch_record(
    nonce: &str,
    bloom: BloomId,
    workpiece: &WorkpieceId,
    scope_revision: Digest,
    candidate: Digest,
) -> DispatchRecord {
    DispatchRecord {
        nonce: Nonce(nonce.to_owned()),
        bloom,
        workpiece: workpiece.clone(),
        scope_revision,
        candidate,
        displayed_digest: candidate,
    }
}

// A hand-built snapshot with one sealed bloom carrying `workpiece` at
// `scope_revision` — the reducer oracle the intake's produced event is folded
// against. reduce_integrate checks membership + evidence binding only, so the
// spec's catalog/base need not be admissible.
fn sealed_snapshot(workpiece: &WorkpieceId, scope_revision: Digest) -> (Snapshot, BloomId) {
    let member = Membership {
        workpiece: workpiece.clone(),
        scope_revision,
        approval: Evidence {
            subject: scope_revision,
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([3; 32]),
        },
    };
    let spec = BloomDraft {
        proposals: vec![member],
        base: Digest::default(),
        stage_catalog: Digest::default(),
        toolchain: Digest::default(),
        policy: Digest::default(),
        budget: Budget::default(),
        forecast: Forecast::default(),
    }
    .seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::new(Digest::default());
    snapshot.active.insert(workpiece.clone(), bloom);
    snapshot.blooms.insert(
        bloom,
        BloomRecord {
            spec,
            status: BloomStatus::Sealed,
            claims: BTreeMap::new(),
            evidence: Vec::new(),
            holds: BTreeSet::new(),
            progress: BTreeMap::new(),
            superseded_by: None,
        },
    );
    (snapshot, bloom)
}

struct SeededClaims(HashMap<String, UploadedEvidence>);

impl EvidenceClaims for SeededClaims {
    fn claim_for(&self, evidence: &EvidenceRef) -> Option<UploadedEvidence> {
        self.0.get(&evidence.nonce.0).cloned()
    }
}

#[derive(Default)]
struct Collector(Vec<Admission>);

impl AdmitSink for Collector {
    fn admit(&mut self, admission: Admission) {
        self.0.push(admission);
    }
}

#[test]
fn a_matching_upload_admits_a_bound_integrate_fact() {
    // The accept path (steps 3–4): a matched nonce + displayed digest normalizes
    // to a `Fact::Integrate` whose claim's evidence validates its candidate — the
    // binding the reducer re-checks. And the order is consumed on accept.
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-return".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let record = dispatch_record("n-1", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-1".to_owned()),
        subject: candidate,
        verdict: StageVerdict::VerificationPassed,
        detail: Digest::from_bytes([7; 32]),
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("a matching upload is admitted");
    };
    let Fact::Integrate { bloom: admitted_bloom, claim } = &admission.event.fact else {
        panic!("the admitted event is a Fact::Integrate");
    };
    assert_eq!(*admitted_bloom, bloom);
    assert_eq!(claim.workpiece, workpiece);
    assert_eq!(claim.candidate, candidate);
    assert!(claim.evidence.validates(&claim.candidate), "evidence binds to the candidate the reducer re-checks");

    // Consume-once: the same nonce no longer resolves, so a replay refuses.
    assert!(matches!(
        admit_uploaded(&mut store, &upload).unwrap(),
        AdmitDecision::Refused(IntakeRefusal::UnknownNonce(_))
    ));
}

#[test]
fn a_parked_upload_admits_a_question_evidence_fact_and_consumes_the_order() {
    // The parked path (ADR-0151): a matched nonce + displayed digest with a
    // `Parked` verdict normalizes to a `Fact::AdmitEvidence` carrying `Question`
    // evidence — never a `Fact::Integrate` — bound to the displayed digest, and
    // the order is consumed (a decision pending is not a stage failure, but the
    // nonce is spent). The reducer folds the pending-decision hold from this.
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-return".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let record = dispatch_record("n-park", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-park".to_owned()),
        subject: candidate,
        verdict: StageVerdict::Parked,
        detail: Digest::from_bytes([8; 32]),
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("a matching parked upload is admitted");
    };
    let Fact::AdmitEvidence { bloom: admitted_bloom, evidence } = &admission.event.fact else {
        panic!("the admitted parked event is a Fact::AdmitEvidence, not an Integrate");
    };
    assert_eq!(*admitted_bloom, bloom);
    assert_eq!(evidence.kind, EvidenceKind::Question);
    assert!(evidence.validates(&candidate), "the question evidence binds to the displayed digest");
    assert_eq!(evidence.detail, Digest::from_bytes([8; 32]), "detail names the produced Question artifact");

    // Consume-once: the nonce is spent, so a replay refuses.
    assert!(matches!(
        admit_uploaded(&mut store, &upload).unwrap(),
        AdmitDecision::Refused(IntakeRefusal::UnknownNonce(_))
    ));
}

#[test]
fn an_unknown_nonce_is_refused() {
    // Nothing recorded — a fabricated upload names no live order.
    let mut store = store();
    let upload = UploadedEvidence {
        nonce: Nonce("n-fabricated".to_owned()),
        subject: Digest::from_bytes([5; 32]),
        verdict: StageVerdict::VerificationPassed,
        detail: Digest::from_bytes([7; 32]),
    };
    assert!(matches!(
        admit_uploaded(&mut store, &upload).unwrap(),
        AdmitDecision::Refused(IntakeRefusal::UnknownNonce(_))
    ));
}

#[test]
fn a_right_nonce_with_the_wrong_digest_is_refused_and_the_order_stays_live() {
    // The trust boundary: a live nonce but a digest other than the one displayed
    // is a lie — refused, and (unlike an accept) the order is NOT consumed, so
    // the honest worker's matching upload can still land.
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-return".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let record = dispatch_record("n-2", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    record_dispatch(&mut store, &record).unwrap();

    let lying = UploadedEvidence {
        nonce: Nonce("n-2".to_owned()),
        subject: Digest::from_bytes([9; 32]),
        verdict: StageVerdict::VerificationPassed,
        detail: Digest::from_bytes([7; 32]),
    };
    match admit_uploaded(&mut store, &lying).unwrap() {
        AdmitDecision::Refused(IntakeRefusal::DigestMismatch { displayed, claimed }) => {
            assert_eq!(displayed, candidate);
            assert_eq!(claimed, Digest::from_bytes([9; 32]));
        }
        other => panic!("expected DigestMismatch, got {other:?}"),
    }
    // The order stayed live: the honest matching upload now accepts.
    let honest = UploadedEvidence { subject: candidate, ..lying };
    assert!(matches!(admit_uploaded(&mut store, &honest).unwrap(), AdmitDecision::Admitted(_)));
}

#[test]
fn dispatch_and_record_submits_then_writes_the_order_row() {
    // Step 2: submitting through the shell records exactly the order's registry
    // row (the linkage the returning evidence is matched by), leaving the core
    // work order unchanged.
    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-return".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let record = dispatch_record("n-dispatch", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);

    let handle = dispatch_and_record(&shell, &mut store, &work_order("n-dispatch"), &record).unwrap();
    assert_eq!(handle, WorkHandle::new(Nonce("n-dispatch".to_owned())));
    // The dispatch reached the executor surface...
    assert_eq!(fake.dispatched_nonces(), vec!["n-dispatch".to_owned()]);
    // ...and the registry row resolves the reducer context by nonce.
    let stored = store.lookup_order("n-dispatch").unwrap().expect("the order was recorded");
    assert_eq!(stored.workpiece, "wp-return");
    assert_eq!(stored.bloom, bloom.0.as_bytes().to_vec());
    assert_eq!(stored.candidate, candidate.as_bytes().to_vec());
    assert_eq!(stored.displayed_digest, candidate.as_bytes().to_vec());
}

#[test]
fn intake_cycle_admits_a_matching_upload_and_the_reducer_integrates_it() {
    // The end-to-end return path (steps 5–6): a completed run's matching upload
    // is pulled, normalized, and admitted; the produced event, folded against a
    // snapshot whose bloom carries the workpiece, integrates. The reducer is the
    // oracle — the intake admits only provenance-and-binding-valid evidence.
    let workpiece = WorkpieceId("wp-return".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let (snapshot, bloom) = sealed_snapshot(&workpiece, scope_revision);
    let candidate = Digest::from_bytes([5; 32]);

    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let mut store = store();
    let record = dispatch_record("n-e2e", bloom, &workpiece, scope_revision, candidate);
    let handle = dispatch_and_record(&shell, &mut store, &work_order("n-e2e"), &record).unwrap();

    // The worker's run completed and uploaded one nonce-named evidence artifact.
    let run_id = fake.seed_run("n-e2e", RunStatus::Completed, Some(RunConclusion::Success));
    fake.seed_run_artifacts(run_id, vec![Artifact { id: 1, name: "evidence-n-e2e-log".to_owned(), size_bytes: 10 }]);

    let mut claims = HashMap::new();
    claims.insert(
        "n-e2e".to_owned(),
        UploadedEvidence {
            nonce: Nonce("n-e2e".to_owned()),
            subject: candidate,
            verdict: StageVerdict::VerificationPassed,
            detail: Digest::from_bytes([7; 32]),
        },
    );
    let claims = SeededClaims(claims);
    let mut sink = Collector::default();

    let report = run_intake_cycle(&mut store, &shell, &[handle], &claims, &mut sink).unwrap();
    assert_eq!((report.completed, report.admitted, report.refused), (1, 1, 0));
    assert_eq!(sink.0.len(), 1);

    // The reducer oracle: the admitted event integrates its member.
    match reduce(&snapshot, &sink.0[0].event).outcome {
        Outcome::Integrated { bloom: integrated, workpiece: member } => {
            assert_eq!(integrated, bloom);
            assert_eq!(member, workpiece);
        }
        other => panic!("expected Integrated, got {other:?}"),
    }
}

#[test]
fn intake_cycle_refuses_a_mismatched_upload_and_the_reducer_is_untouched() {
    // The refuse path end-to-end: a completed run's upload whose bound digest is
    // not the displayed one is refused; nothing reaches the sink (so nothing
    // reaches the reducer), and the order stays live.
    let workpiece = WorkpieceId("wp-return".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let candidate = Digest::from_bytes([5; 32]);

    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let record = dispatch_record("n-bad", bloom, &workpiece, scope_revision, candidate);
    let handle = dispatch_and_record(&shell, &mut store, &work_order("n-bad"), &record).unwrap();

    let run_id = fake.seed_run("n-bad", RunStatus::Completed, Some(RunConclusion::Success));
    fake.seed_run_artifacts(run_id, vec![Artifact { id: 1, name: "evidence-n-bad-log".to_owned(), size_bytes: 10 }]);

    let mut claims = HashMap::new();
    claims.insert(
        "n-bad".to_owned(),
        UploadedEvidence {
            nonce: Nonce("n-bad".to_owned()),
            subject: Digest::from_bytes([9; 32]),
            verdict: StageVerdict::VerificationPassed,
            detail: Digest::from_bytes([7; 32]),
        },
    );
    let claims = SeededClaims(claims);
    let mut sink = Collector::default();

    let report = run_intake_cycle(&mut store, &shell, &[handle], &claims, &mut sink).unwrap();
    assert_eq!((report.completed, report.admitted, report.refused), (1, 0, 1));
    assert!(sink.0.is_empty(), "a refused upload never reaches the reducer");
    // The order stayed live — the mismatch did not consume it.
    assert!(store.lookup_order("n-bad").unwrap().is_some());
}
