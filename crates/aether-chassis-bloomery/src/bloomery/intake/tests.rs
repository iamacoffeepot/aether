//! Evidence-intake tests (#3502): the broker accept-gate (nonce + digest match,
//! consume-once), the dispatch-record write, and the pull-loop cycle driven
//! end-to-end with the reducer as the oracle.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use aether_bloomery::{
    BloomDraft, BloomId, BloomRecord, BloomStatus, Budget, Decision, Digest, Event, Evidence, EvidenceKind,
    EvidenceRef, ExecutionStatus, Fact, Forecast, IdempotencyKey, Membership, NetworkProfile, Nonce, Outcome, Snapshot,
    StageCatalog, StageId, Transformation, WorkHandle, WorkOrder, WorkpieceId, reduce,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{
    ActionsExecutor, Artifact, ExecutorError, GithubError, LaneWorkflows, RunConclusion, RunStatus, StageVerdict,
};

use super::{
    Admission, AdmitDecision, AdmitSink, DispatchError, DispatchRecord, EvidenceClaims, IntakeRefusal,
    UploadedEvidence, admit_uploaded, dispatch_and_record, record_dispatch, run_intake_cycle,
};
use crate::bloomery::{ExecutorPortError, ExecutorShell, LocalExecutorError};
use crate::store::{SqliteStore, StoreBackend};

const WORKFLOW: &str = "bloomery-transform.yml";
const MODEL_WORKFLOW: &str = "bloomery-transform-model.yml";
const PINNED_REF: &str = "refs/heads/main";

fn store() -> SqliteStore {
    SqliteStore::open(":memory:").unwrap()
}

fn shell(fake: FakeGithub) -> ExecutorShell {
    // The dispatched orders check out `[0xC0; 32]`; seed its correspondence so
    // `submit` resolves the subject (the fake's store is shared across clones).
    fake.seed_git_object(&Digest::from_bytes([0xC0; 32]));
    let lanes = LaneWorkflows { mechanical: WORKFLOW.to_owned(), model: MODEL_WORKFLOW.to_owned() };
    ExecutorShell::new(Arc::new(ActionsExecutor::new(fake.clone(), Arc::new(fake), lanes, PINNED_REF)))
}

fn work_order(nonce: &str) -> WorkOrder {
    WorkOrder {
        transformation: Transformation {
            command: "verify.clippy".to_owned(),
            inputs: Vec::new(),
            checkout: Digest::from_bytes([0xC0; 32]),
            outputs: Vec::new(),
            image: "iama/verify:1".to_owned(),
            limits: Budget::default(),
            network: NetworkProfile::None,
            description: None,
            model: None,
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
        // The terminal per-member stage (ADR-0153: Verify), so a resolving
        // verdict admits as a `Fact::Integrate` — the existing accept/refuse
        // tests' oracle. The non-terminal `AttemptCompleted` path is exercised
        // by its own test.
        stage: StageId::Verify,
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
            integration: None,
            aggregate_rolls: 0,
            review_park: None,
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
        candidate: None,
        findings: None,
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
        candidate: None,
        findings: None,
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
        candidate: None,
        findings: None,
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
        candidate: None,
        findings: None,
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
            candidate: None,
            findings: None,
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
            candidate: None,
            findings: None,
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

#[test]
fn a_pending_handle_is_reported_and_neither_completed_nor_admitted() {
    // A run that hasn't resolved yet (#3635) is skipped as before, but now
    // surfaces in `report.pending` with its observed status — the data the
    // executor reactor's staleness sweep reads, rather than silence.
    let workpiece = WorkpieceId("wp-pending".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let candidate = Digest::from_bytes([5; 32]);

    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let record = dispatch_record("n-pending", bloom, &workpiece, scope_revision, candidate);
    let handle = dispatch_and_record(&shell, &mut store, &work_order("n-pending"), &record).unwrap();

    let _ = fake.seed_run("n-pending", RunStatus::InProgress, None);

    let claims = SeededClaims(HashMap::new());
    let mut sink = Collector::default();

    let report = run_intake_cycle(&mut store, &shell, &[handle], &claims, &mut sink).unwrap();
    assert_eq!((report.completed, report.admitted, report.refused), (0, 0, 0));
    assert_eq!(report.pending, vec![(Nonce("n-pending".to_owned()), ExecutionStatus::Running)]);
    assert!(sink.0.is_empty());
}

// A sealed single-member bloom driven through the *reducer's* seal, so the
// member's stage cursor is seeded at the entry stage and the entry dispatch is
// emitted (#3505) — the state the per-member advance composition starts from.
fn sealed_via_reducer(workpiece: &WorkpieceId, scope_revision: Digest) -> (Snapshot, BloomId) {
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
        stage_catalog: StageCatalog::line_digest(),
        ..BloomDraft::default()
    }
    .seal();
    let bloom = spec.id();
    let snapshot = Snapshot::new(Digest::default());
    let seal = Event { idempotency_key: IdempotencyKey("seal".to_owned()), fact: Fact::Seal(spec) };
    let decisions = reduce(&snapshot, &seal);
    let snapshot = snapshot.apply(&seal, &decisions);
    (snapshot, bloom)
}

#[test]
fn a_non_terminal_construct_result_admits_attempt_completed_and_the_reducer_advances_to_verify() {
    // The per-member line composition (#3505), end to end over the intake broker
    // and the reducer: a Construct dispatch's passing result admits as a
    // Fact::AttemptCompleted (not a Fact::Integrate — Construct is non-terminal),
    // and folding it through the reducer advances the member Construct → Verify and
    // dispatches the next attempt.
    let mut store = store();
    let workpiece = WorkpieceId("wp-line".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let (snapshot, bloom) = sealed_via_reducer(&workpiece, scope_revision);
    // Seal seeded the member at the entry stage.
    assert_eq!(
        snapshot.blooms.get(&bloom).unwrap().progress.get(&workpiece).unwrap().stage,
        StageId::Construct,
        "seal seeds the member at Construct",
    );

    // Record a Construct dispatch order and admit a passing result for it.
    let candidate = Digest::from_bytes([5; 32]);
    let mut record = dispatch_record("n-c", bloom, &workpiece, scope_revision, candidate);
    record.stage = StageId::Construct;
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-c".to_owned()),
        subject: candidate,
        verdict: StageVerdict::VerificationPassed,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("a matching Construct upload is admitted");
    };
    let Fact::AttemptCompleted { stage, passed, .. } = &admission.event.fact else {
        panic!("a non-terminal stage admits AttemptCompleted, not Integrate");
    };
    assert_eq!(*stage, StageId::Construct);
    assert!(*passed, "a VerificationPassed verdict passes the gate");

    // Fold it through the reducer: the member advances to Verify and dispatches it.
    let decisions = reduce(&snapshot, &admission.event);
    assert!(matches!(
        decisions.outcome,
        Outcome::AttemptAdvanced { from: StageId::Construct, to: StageId::Verify, .. }
    ));
    let next = snapshot.apply(&admission.event, &decisions);
    assert_eq!(
        next.blooms.get(&bloom).unwrap().progress.get(&workpiece).unwrap().stage,
        StageId::Verify,
        "the member advanced to Verify",
    );
    assert!(
        decisions
            .effects
            .iter()
            .any(|effect| matches!(effect, Decision::DispatchAttempt { stage: StageId::Verify, .. })),
        "the advance dispatches the next (Verify) attempt",
    );
}

#[test]
fn a_failing_terminal_verify_admits_attempt_completed_not_integrate() {
    // Tripwire: the completion gate applies across the whole member line, the
    // terminal Verify included (ADR-0153). A *failing* Verify upload admits as a
    // Fact::AttemptCompleted { passed: false } — never a Fact::Integrate — so the
    // reducer routes it into the Refine repair re-entry rather than recording a
    // failing verify as resolved.
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-verify".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    // dispatch_record's default stage is the terminal Verify.
    let record = dispatch_record("n-ver", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    assert_eq!(record.stage, StageId::Verify, "the record is at the terminal Verify stage");
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-ver".to_owned()),
        subject: candidate,
        verdict: StageVerdict::VerificationFailed,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("a matching failing-verify upload is admitted (the gate decides its fate, not the broker)");
    };
    let Fact::AttemptCompleted { stage, passed, .. } = &admission.event.fact else {
        panic!("a failing terminal Verify admits AttemptCompleted, not Integrate");
    };
    assert_eq!(*stage, StageId::Verify);
    assert!(!*passed, "a VerificationFailed verdict fails the gate");

    // The order is consumed on accept, like any admitted result.
    assert!(matches!(
        admit_uploaded(&mut store, &upload).unwrap(),
        AdmitDecision::Refused(IntakeRefusal::UnknownNonce(_))
    ));
}

// ADR-0153 — an AggregateReview order's verdict admits as
// Fact::AggregateReviewCompleted: a bloom-level fact with an empty implication
// (the reducer expands it to every member on a fail). A failing verdict's
// findings persist bloom-scoped under the empty workpiece key — the row every
// re-opened member's Refine dispatch reads — and a passing verdict clears it.
#[test]
fn an_aggregate_review_verdict_admits_a_bloom_level_completion() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let tree = Digest::from_bytes([30; 32]);
    let mut record = dispatch_record("n-agg", bloom, &WorkpieceId(String::new()), tree, tree);
    record.stage = StageId::AggregateReview;
    record_dispatch(&mut store, &record).unwrap();

    let failing = UploadedEvidence {
        nonce: Nonce("n-agg".to_owned()),
        subject: tree,
        verdict: StageVerdict::ReviewFinding,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: Some("pillar 2: the members disagree about the tick order".to_owned()),
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &failing).unwrap() else {
        panic!("a matching aggregate verdict is admitted");
    };
    let Fact::AggregateReviewCompleted { bloom: reviewed, passed, implicated, .. } = &admission.event.fact else {
        panic!("an AggregateReview order admits AggregateReviewCompleted, got {:?}", admission.event.fact);
    };
    assert_eq!(*reviewed, bloom);
    assert!(!*passed, "a ReviewFinding verdict fails the gate");
    assert!(implicated.is_empty(), "the intake names no members — the reducer expands the empty implication");
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), "").unwrap().as_deref(),
        Some("pillar 2: the members disagree about the tick order"),
        "a failing aggregate verdict's findings persist bloom-scoped",
    );

    // A second (delta-confirm) order whose passing verdict clears the bloom row.
    let mut second = dispatch_record("n-agg2", bloom, &WorkpieceId(String::new()), tree, tree);
    second.stage = StageId::AggregateReview;
    record_dispatch(&mut store, &second).unwrap();
    let passing = UploadedEvidence {
        nonce: Nonce("n-agg2".to_owned()),
        subject: tree,
        verdict: StageVerdict::Approved,
        detail: Digest::from_bytes([8; 32]),
        candidate: None,
        findings: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &passing).unwrap() else {
        panic!("the passing aggregate verdict is admitted");
    };
    assert!(matches!(&admission.event.fact, Fact::AggregateReviewCompleted { passed: true, .. }));
    assert_eq!(store.lookup_review_findings(bloom.0.as_bytes(), "").unwrap(), None, "the pass clears the bloom row");
}

// ADR-0153 — a *completely attributed* failing aggregate verdict narrows the
// implication to the owning members and slices the findings per member: the
// owner's Refine re-entry reads its own slice, the un-implicated member stays
// resolved, and the bloom row freezes the full set. A later failing verdict
// (the delta-confirm) appends under its own label — the frozen set the members
// were re-opened against is never clobbered.
#[test]
fn attributed_aggregate_findings_narrow_the_implication_and_slice_per_member() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let tree = Digest::from_bytes([30; 32]);
    store.record_dispatch_description(bloom.0.as_bytes(), "wp-a", "build the widget").unwrap();
    store.record_dispatch_description(bloom.0.as_bytes(), "wp-b", "wire the widget").unwrap();
    let mut record = dispatch_record("n-agg", bloom, &WorkpieceId(String::new()), tree, tree);
    record.stage = StageId::AggregateReview;
    record_dispatch(&mut store, &record).unwrap();

    let findings = "[wp-a] The widget leaks its handle.\n\n[wp-a] The tick order inverts.";
    let failing = UploadedEvidence {
        nonce: Nonce("n-agg".to_owned()),
        subject: tree,
        verdict: StageVerdict::ReviewFinding,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: Some(findings.to_owned()),
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &failing).unwrap() else {
        panic!("a matching aggregate verdict is admitted");
    };
    let Fact::AggregateReviewCompleted { implicated, .. } = &admission.event.fact else {
        panic!("an AggregateReview order admits AggregateReviewCompleted, got {:?}", admission.event.fact);
    };
    assert_eq!(implicated.as_slice(), &[WorkpieceId("wp-a".to_owned())], "only the owning member is implicated");
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), "wp-a").unwrap().as_deref(),
        Some(findings),
        "the owner's blocks slice into its member row",
    );
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), "wp-b").unwrap(),
        None,
        "the un-implicated member gets no slice",
    );
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), "").unwrap().as_deref(),
        Some(findings),
        "the bloom row freezes the full set",
    );

    let mut second = dispatch_record("n-agg2", bloom, &WorkpieceId(String::new()), tree, tree);
    second.stage = StageId::AggregateReview;
    record_dispatch(&mut store, &second).unwrap();
    let delta_fail = UploadedEvidence {
        nonce: Nonce("n-agg2".to_owned()),
        subject: tree,
        verdict: StageVerdict::ReviewFinding,
        detail: Digest::from_bytes([8; 32]),
        candidate: None,
        findings: Some("[wp-a] Still leaking.".to_owned()),
    };
    assert!(matches!(admit_uploaded(&mut store, &delta_fail).unwrap(), AdmitDecision::Admitted(_)));
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), "").unwrap().as_deref(),
        Some(format!("{findings}\n\n## Delta-confirm findings\n\n[wp-a] Still leaking.").as_str()),
        "the delta-confirm's failure appends under its own label, keeping the frozen head",
    );
}

#[test]
fn an_out_of_line_stage_is_refused_and_the_order_stays_live() {
    // A well-formed dispatch only ever carries a dispatched member stage
    // (Construct / Verify / the repair-only Refine); an order at any other
    // stage — the retired member Review included (ADR-0153) — is corrupt. It is
    // refused as OutOfLineStage rather than folded into the member's resolution,
    // and (like a digest mismatch) the order is NOT consumed.
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-off".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let mut record = dispatch_record("n-off", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    record.stage = StageId::AggregateVerify;
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-off".to_owned()),
        subject: candidate,
        verdict: StageVerdict::VerificationPassed,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: None,
    };
    match admit_uploaded(&mut store, &upload).unwrap() {
        AdmitDecision::Refused(IntakeRefusal::OutOfLineStage(stage)) => {
            assert_eq!(stage, StageId::AggregateVerify);
        }
        other => panic!("expected OutOfLineStage refusal, got {other:?}"),
    }
    // The order stayed live — the refusal precedes the consume, so a corrected
    // dispatch could still land it.
    assert!(store.lookup_order("n-off").unwrap().is_some(), "an out-of-line refusal leaves the order live");
}

// Tripwire: the attempt-artifact name codec round-trips (#3505). The wrapper
// encodes an attempt result into an artifact name and the pull-side
// `NameEvidenceClaims` decodes it from the reference; the two must be inverse, and
// the nonce is authoritative from the reference (what the port matched the run
// by), not the name's trailing segment. A drift in either half strands every
// returning attempt result at the broker.
#[test]
fn attempt_artifact_name_round_trips_through_name_evidence_claims() {
    use aether_bloomery::EvidenceRef;

    use super::{NameEvidenceClaims, attempt_artifact_name};

    let claims = NameEvidenceClaims;
    let cases = [
        (StageVerdict::Approved, 5u8, 9u8),
        (StageVerdict::VerificationPassed, 200, 201),
        (StageVerdict::VerificationFailed, 0, 255),
        (StageVerdict::ReviewFinding, 42, 7),
        (StageVerdict::Parked, 17, 18),
    ];
    for (verdict, subject_seed, detail_seed) in cases {
        let nonce = Nonce("dispatch-42".to_owned());
        let subject = Digest::from_bytes([subject_seed; 32]);
        let detail = Digest::from_bytes([detail_seed; 32]);
        let name = attempt_artifact_name(&nonce, &subject, verdict, &detail);
        let reference =
            EvidenceRef { name, nonce: nonce.clone(), artifact_id: 1, size_bytes: 10, candidate: None, findings: None };

        let decoded = claims.claim_for(&reference).expect("a well-formed attempt name decodes");
        assert_eq!(decoded.nonce, nonce);
        assert_eq!(decoded.subject, subject);
        assert_eq!(decoded.verdict, verdict);
        assert_eq!(decoded.detail, detail);
    }

    // A non-attempt artifact name (a study record, a stray log) is skipped, not
    // mis-decoded into a bogus attempt result.
    let stray = EvidenceRef {
        name: "study.dispatch-42.log".to_owned(),
        nonce: Nonce("dispatch-42".to_owned()),
        artifact_id: 2,
        size_bytes: 3,
        candidate: None,
        findings: None,
    };
    assert!(claims.claim_for(&stray).is_none(), "a non-attempt name yields no claim");
}

// Tripwire: 429 is a rate-limit, not a permanent refusal — the one classification
// edge that would silently regress into hammering GitHub if it flipped, since
// every other 4xx and every 5xx sit on opposite, unambiguous sides of the split.
#[test]
fn dispatch_error_is_permanent_classifies_github_status_by_code() {
    let github_status = |status: u16| {
        DispatchError::Submit(ExecutorPortError::Actions(ExecutorError::Github(GithubError::Status {
            status,
            body: "refused".to_owned(),
        })))
    };

    assert!(github_status(404).is_permanent(), "a 404 wrong-workflow refusal is permanent");
    assert!(github_status(422).is_permanent(), "a 422 contract-mismatch refusal is permanent");
    assert!(!github_status(429).is_permanent(), "429 is a rate-limit, not a permanent refusal");
    assert!(!github_status(500).is_permanent(), "a 5xx is transient");
    assert!(!github_status(399).is_permanent(), "below the 4xx range is transient");
}

#[test]
fn dispatch_error_is_permanent_is_false_for_every_non_status_fault() {
    let transport = DispatchError::Submit(ExecutorPortError::Actions(ExecutorError::Github(GithubError::Transport(
        "connection reset".to_owned(),
    ))));
    let decode = DispatchError::Submit(ExecutorPortError::Actions(ExecutorError::Github(GithubError::Decode(
        "bad body".to_owned(),
    ))));
    let pagination =
        DispatchError::Submit(ExecutorPortError::Actions(ExecutorError::Github(GithubError::PaginationExhausted {
            what: "runs".to_owned(),
        })));
    let no_run =
        DispatchError::Submit(ExecutorPortError::Actions(ExecutorError::NoRunForNonce(Nonce("dispatch-1".to_owned()))));
    let local = DispatchError::Submit(ExecutorPortError::Local(LocalExecutorError::NoRunForNonce(Nonce(
        "dispatch-1".to_owned(),
    ))));
    let store = DispatchError::Store(rusqlite::Error::QueryReturnedNoRows);

    for error in [transport, decode, pagination, no_run, local, store] {
        assert!(!error.is_permanent(), "{error} is a transient fault, not permanent");
    }
}

// #3656 / ADR-0153 — a failing verify's findings (the mechanical failure
// output) persist keyed by the member (what the Refine repair re-entry is
// directed by), and a passing verify clears the stale row. Catches both leaks:
// findings never recorded (a blind re-entry), and stale findings surviving the
// pass that resolved them (a poisoned later prompt).
#[test]
fn verify_findings_persist_on_a_failing_verify_and_clear_on_a_pass() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-findings".to_owned());
    let candidate = Digest::from_bytes([5; 32]);

    let upload = |nonce: &str, verdict: StageVerdict, findings: Option<&str>| UploadedEvidence {
        nonce: Nonce(nonce.to_owned()),
        subject: candidate,
        verdict,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: findings.map(str::to_owned),
    };

    record_dispatch(&mut store, &dispatch_record("n-f1", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate))
        .unwrap();
    let AdmitDecision::Admitted(_) = admit_uploaded(
        &mut store,
        &upload("n-f1", StageVerdict::VerificationFailed, Some("clippy: unused variable `head`")),
    )
    .unwrap() else {
        panic!("the failing verify admits");
    };
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), &workpiece.0).unwrap().as_deref(),
        Some("clippy: unused variable `head`"),
        "a failing verify's findings persist for the re-entry",
    );

    record_dispatch(&mut store, &dispatch_record("n-f2", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate))
        .unwrap();
    let AdmitDecision::Admitted(_) =
        admit_uploaded(&mut store, &upload("n-f2", StageVerdict::VerificationPassed, None)).unwrap()
    else {
        panic!("the passing verify admits");
    };
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), &workpiece.0).unwrap(),
        None,
        "a passing verify clears the stale findings",
    );
}
