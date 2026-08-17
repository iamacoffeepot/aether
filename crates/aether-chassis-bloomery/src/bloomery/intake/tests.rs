//! Evidence-intake tests (#3502): the broker accept-gate (nonce + digest match,
//! consume-once), the dispatch-record write, and the pull-loop cycle driven
//! end-to-end with the reducer as the oracle.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{Debug, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aether_bloomery::{
    BloomDraft, BloomId, BloomRecord, BloomStatus, ConfigRegistry, Decision, Digest, Event, Evidence, EvidenceKind,
    EvidenceRef, ExecutionLimits, ExecutionStatus, Fact, Forecast, IdempotencyKey, Membership, NetworkProfile, Nonce,
    Outcome, ResolvedConfigs, Snapshot, SpendWindow, StageCatalog, StageId, StageVerdict, Transformation,
    VerifyFailure, VerifyFailureSet, WorkHandle, WorkpieceId, reduce,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{
    ActionsExecutor, Artifact, ExecutorError, GithubError, LaneWorkflows, RunConclusion, RunStatus,
};
use aether_data::wire::from_bytes;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::with_default;
use tracing::{Event as TracingEvent, Metadata, Subscriber};

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

fn transformation() -> Transformation {
    Transformation {
        command: "verify.clippy".to_owned(),
        inputs: Vec::new(),
        checkout: Digest::from_bytes([0xC0; 32]),
        diff_base: None,
        outputs: Vec::new(),
        image: "iama/verify:1".to_owned(),
        limits: ExecutionLimits { wall_clock_secs: 3_600 },
        network: NetworkProfile::None,
        description: None,
        model: None,
    }
}

/// A fixed Unix-millisecond clock reading the dispatch fixtures record against,
/// so a deadline assertion never depends on when the suite ran.
const NOW_UNIX_MILLIS: u64 = 1_700_000_000_000;

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
        profile: StageCatalog::profile_of(StageId::Construct),
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
        transformation: transformation(),
        configs: ConfigRegistry::default(),
    }
}

// A hand-built snapshot with one sealed bloom carrying `workpiece` at
// `scope_revision` — the reducer oracle the intake's produced event is folded
// against. reduce_integrate checks membership + evidence binding only, so the
// spec's catalog/base need not be admissible.
fn sealed_snapshot(workpiece: &WorkpieceId, scope_revision: Digest) -> (Snapshot, BloomId) {
    let mut member = Membership {
        workpiece: workpiece.clone(),
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: Evidence {
            subject: Digest::default(),
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([3; 32]),
        },
    };
    // The approval binds the member's whole subject (ADR-0174).
    member.approval.subject = member.subject();
    let spec = BloomDraft {
        proposals: vec![member],
        base: Digest::default(),
        configs: ConfigRegistry::default(),
        forecast: Forecast::default(),
    }
    .seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::new(Digest::default());
    snapshot.active.insert(workpiece.clone(), bloom);
    snapshot.blooms.insert(
        bloom,
        BloomRecord {
            stage_catalog: StageCatalog::line(),
            spec,
            status: BloomStatus::Sealed,
            claims: BTreeMap::new(),
            evidence: Vec::new(),
            holds: BTreeSet::new(),
            progress: BTreeMap::new(),
            wedged: BTreeMap::new(),
            dispatches: BTreeMap::new(),
            integration: None,
            aggregate_rolls: 0,
            aggregate_verify_rolls: 0,
            landing_rolls: 0,
            resolved_head: None,
            review_park: None,
            verify_proofs: BTreeMap::new(),
            verify_reuses: Vec::new(),
            aggregate_fault: None,
            composition_findings: Vec::new(),
            adjudications: Vec::new(),
            operator_repairs: Vec::new(),
            operator_hold: None,
            deferred_dispatches: BTreeSet::new(),
            dependencies: Vec::new(),
            host_faults: BTreeMap::new(),
            vehicles: BTreeMap::new(),
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

fn sink_has_study_evidence(sink: &Collector) -> bool {
    sink.0.iter().any(|admission| {
        matches!(&admission.event.fact, Fact::AdmitEvidence { evidence, .. } if evidence.kind == EvidenceKind::StudyRecord)
    })
}

struct CapturingSubscriber {
    events: Arc<Mutex<Vec<String>>>,
    next_span: AtomicU64,
}

impl CapturingSubscriber {
    fn new(events: Arc<Mutex<Vec<String>>>) -> Self {
        Self { events, next_span: AtomicU64::new(1) }
    }
}

#[derive(Default)]
struct CapturedFields(String);

impl Visit for CapturedFields {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        write!(&mut self.0, "{}={value:?} ", field.name()).expect("writing to a String cannot fail");
    }
}

impl Subscriber for CapturingSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &TracingEvent<'_>) {
        let mut fields = CapturedFields::default();
        event.record(&mut fields);
        self.events.lock().unwrap().push(format!(
            "{} {} {}",
            event.metadata().level(),
            event.metadata().target(),
            fields.0
        ));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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

    let handle = dispatch_and_record(&shell, &mut store, &record, NOW_UNIX_MILLIS).unwrap();
    assert_eq!(handle, WorkHandle::new(Nonce("n-dispatch".to_owned())));
    // The dispatch reached the executor surface...
    assert_eq!(fake.dispatched_nonces(), vec!["n-dispatch".to_owned()]);
    // ...and the registry row resolves the reducer context by nonce.
    let stored = store.lookup_order("n-dispatch").unwrap().expect("the order was recorded");
    assert_eq!(stored.workpiece, "wp-return");
    assert_eq!(stored.bloom, bloom.0.as_bytes().to_vec());
    assert_eq!(stored.candidate, candidate.as_bytes().to_vec());
    assert_eq!(stored.displayed_digest, candidate.as_bytes().to_vec());
    // Tripwire: the deadline is the record instant plus the *sealed* limit the
    // order's own transformation carries (ADR-0177), in Unix milliseconds. A
    // coordinator-local or defaulted number here would let two blooms sealing
    // the same catalog terminate differently, which is the property the sealed
    // catalog exists to deny.
    assert_eq!(stored.deadline_unix_millis, NOW_UNIX_MILLIS + 3_600_000);
}

#[test]
fn a_dispatch_deadline_is_absolute_and_a_re_record_does_not_move_it() {
    // Restart recovery re-records nothing, but a transient redrive can re-reach
    // the same nonce. The order's allowance is spent from its first record, so
    // the second one must change nothing — an extension per redrive is how a
    // hung order outlives every restart.
    let fake = FakeGithub::new();
    let shell = shell(fake);
    let mut store = store();
    let record = dispatch_record(
        "n-redrive",
        BloomId(Digest::from_bytes([1; 32])),
        &WorkpieceId("wp-return".to_owned()),
        Digest::from_bytes([2; 32]),
        Digest::from_bytes([5; 32]),
    );

    dispatch_and_record(&shell, &mut store, &record, NOW_UNIX_MILLIS).unwrap();
    dispatch_and_record(&shell, &mut store, &record, NOW_UNIX_MILLIS + 600_000).unwrap();

    assert_eq!(
        store.lookup_order("n-redrive").unwrap().expect("the order is outstanding").deadline_unix_millis,
        NOW_UNIX_MILLIS + 3_600_000,
        "the redrive kept the deadline the first record computed",
    );
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
    let handle = dispatch_and_record(&shell, &mut store, &record, NOW_UNIX_MILLIS).unwrap();

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
            failed_verifiers: VerifyFailureSet::EMPTY,
            cost: None,
            calls: None,
        },
    );
    let claims = SeededClaims(claims);
    let mut sink = Collector::default();

    let report = run_intake_cycle(&mut store, &shell, &[handle], &claims, None, &mut sink).unwrap();
    assert_eq!((report.completed, report.admitted, report.refused), (1, 1, 0));
    assert_eq!(sink.0.len(), 1);

    // The reducer oracle: the admitted event integrates its member.
    match reduce(&snapshot, &sink.0[0].event, &ResolvedConfigs::default(), &SpendWindow::default()).outcome {
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
    let handle = dispatch_and_record(&shell, &mut store, &record, NOW_UNIX_MILLIS).unwrap();

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
            failed_verifiers: VerifyFailureSet::EMPTY,
            cost: None,
            calls: None,
        },
    );
    let claims = SeededClaims(claims);
    let mut sink = Collector::default();

    let events = Arc::new(Mutex::new(Vec::new()));
    let report = with_default(CapturingSubscriber::new(events.clone()), || {
        run_intake_cycle(&mut store, &shell, &[handle], &claims, None, &mut sink).unwrap()
    });
    assert_eq!((report.completed, report.admitted, report.refused), (1, 0, 1));
    assert!(sink.0.is_empty(), "a refused upload never reaches the reducer");
    let events = events.lock().unwrap().join("\n");
    assert!(events.contains("aether_chassis_bloomery::intake"), "the refusal uses the intake target: {events}");
    assert!(events.contains("nonce=n-bad"), "the refusal names the stranded order: {events}");
    assert!(events.contains("DigestMismatch"), "the refusal explains why intake rejected the upload: {events}");
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
    let handle = dispatch_and_record(&shell, &mut store, &record, NOW_UNIX_MILLIS).unwrap();

    let _ = fake.seed_run("n-pending", RunStatus::InProgress, None);

    let claims = SeededClaims(HashMap::new());
    let mut sink = Collector::default();

    let report = run_intake_cycle(&mut store, &shell, &[handle], &claims, None, &mut sink).unwrap();
    assert_eq!((report.completed, report.admitted, report.refused), (0, 0, 0));
    assert_eq!(report.pending, vec![(Nonce("n-pending".to_owned()), ExecutionStatus::Running)]);
    assert!(sink.0.is_empty());
}

// A sealed single-member bloom driven through the *reducer's* seal, so the
// member's stage cursor is seeded at the entry stage and the entry dispatch is
// emitted (#3505) — the state the per-member advance composition starts from.
fn sealed_via_reducer(workpiece: &WorkpieceId, scope_revision: Digest) -> (Snapshot, BloomId) {
    let mut member = Membership {
        workpiece: workpiece.clone(),
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: Evidence {
            subject: Digest::default(),
            kind: EvidenceKind::Approval,
            detail: Digest::from_bytes([3; 32]),
        },
    };
    // The approval binds the member's whole subject (ADR-0174).
    member.approval.subject = member.subject();
    let spec = BloomDraft { proposals: vec![member], base: Digest::default(), ..BloomDraft::default() }.seal();
    let bloom = spec.id();
    let snapshot = Snapshot::new(Digest::default());
    let seal = Event { idempotency_key: IdempotencyKey("seal".to_owned()), fact: Fact::Seal(spec) };
    let decisions = reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default());
    let snapshot = snapshot.apply(&seal, &decisions, &ResolvedConfigs::default());
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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
    let decisions = reduce(&snapshot, &admission.event, &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(matches!(
        decisions.outcome,
        Outcome::AttemptAdvanced { from: StageId::Construct, to: StageId::Verify, .. }
    ));
    let next = snapshot.apply(&admission.event, &decisions, &ResolvedConfigs::default());
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
fn a_reconcile_result_admits_attempt_completed_not_out_of_line() {
    // ADR-0189 — Reconcile is off MEMBER_LINE (next_member_stage is None) the
    // same way Refine is. Routing only on a successor, or only naming Refine
    // as the exception, refuses a completed reconcile as OutOfLineStage: the
    // upload stays live, AttemptCompleted is never journaled, and the member
    // never reaches Verify. The broker must admit it as AttemptCompleted.
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-overlap".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let mut record = dispatch_record("n-rec", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    record.stage = StageId::Reconcile;
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-rec".to_owned()),
        subject: candidate,
        verdict: StageVerdict::VerificationPassed,
        detail: Digest::from_bytes([7; 32]),
        candidate: Some(aether_bloomery::CandidateRef {
            tree: Digest::from_bytes([8; 32]),
            checkout: Digest::from_bytes([9; 32]),
        }),
        findings: None,
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("a matching Reconcile upload is admitted");
    };
    let Fact::AttemptCompleted { stage, passed, .. } = &admission.event.fact else {
        panic!("Reconcile admits AttemptCompleted, not an out-of-line refusal");
    };
    assert_eq!(*stage, StageId::Reconcile);
    assert!(*passed, "a VerificationPassed verdict passes the gate");
    assert!(store.lookup_order("n-rec").unwrap().is_none(), "the admitted order is consumed");
}

#[test]
fn a_failing_terminal_verify_admits_typed_verify_failed_not_integrate() {
    // ADR-0178: a failing Verify upload admits through its dedicated appended
    // fact with the exact typed set — never Integrate or the stage-polymorphic
    // AttemptCompleted path.
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
        failed_verifiers: VerifyFailureSet::one(VerifyFailure::Clippy),
        cost: None,
        calls: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("a matching failing-verify upload is admitted (the gate decides its fate, not the broker)");
    };
    let Fact::VerifyFailed { failed_verifiers, evidence, .. } = &admission.event.fact else {
        panic!("a failing terminal Verify admits VerifyFailed, not Integrate");
    };
    assert_eq!(*failed_verifiers, VerifyFailureSet::one(VerifyFailure::Clippy));
    assert_eq!(evidence.subject, candidate, "the fact retains the intake-validated evidence binding");

    // The order is consumed on accept, like any admitted result.
    assert!(matches!(
        admit_uploaded(&mut store, &upload).unwrap(),
        AdmitDecision::Refused(IntakeRefusal::UnknownNonce(_))
    ));
}

// Tripwire: the fail-closed evidence of a run that died before the umbrella
// judged anything carries an empty verifier set, and the broker has to let it
// through — the reducer reads it as "unjudged" and re-runs Verify. Refusing it
// here strands the attempt behind its execution limit, which is what an hour of
// a production bloom's wall clock went to.
#[test]
fn an_unjudged_verify_naming_no_verifier_is_admitted_rather_than_refused() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-unjudged".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let record = dispatch_record("n-unjudged", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    assert_eq!(record.stage, StageId::Verify);
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-unjudged".to_owned()),
        subject: candidate,
        verdict: StageVerdict::VerificationFailed,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: None,
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("an unjudged failing-verify upload is admitted; the reducer decides what it means");
    };
    let Fact::VerifyFailed { failed_verifiers, .. } = &admission.event.fact else {
        panic!("an unjudged terminal Verify still admits VerifyFailed");
    };
    assert_eq!(*failed_verifiers, VerifyFailureSet::EMPTY, "the empty set reaches the reducer intact");
    assert!(store.lookup_order("n-unjudged").unwrap().is_none(), "the admitted order is consumed");
}

#[test]
fn a_preflight_only_verify_admits_as_a_host_fault_not_a_candidate_failure() {
    // The plausible bug: a missing gate tool arrives as VerifyFailed with
    // `verify.preflight`, so the reducer spends a repair roll and dispatches
    // Refine against a candidate nobody judged (#5020).
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-preflight".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let record = dispatch_record("n-preflight", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    record_dispatch(&mut store, &record).unwrap();

    let findings = "Verification did not run.\n\n- `jscpd` — npm install -g jscpd";
    let upload = UploadedEvidence {
        nonce: Nonce("n-preflight".to_owned()),
        subject: candidate,
        verdict: StageVerdict::VerificationFailed,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: Some(findings.to_owned()),
        failed_verifiers: VerifyFailureSet::one(VerifyFailure::Preflight),
        cost: None,
        calls: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("a preflight-only verify is admitted so the reducer can hold it");
    };
    let Fact::VerifyHostFault { findings: held, evidence, workpiece: held_wp, .. } = &admission.event.fact else {
        panic!("a preflight-only set must journal as VerifyHostFault, got {:?}", admission.event.fact);
    };
    assert_eq!(held, findings, "the missing tools ride the fact verbatim");
    assert_eq!(held_wp, &workpiece);
    assert_eq!(evidence.subject, candidate);
}

// A nonempty typed set is only a failed member Verify or AggregateVerify. A
// passing verify, a parked verify, a non-verify member, a passing aggregate
// verify, or a failing aggregate review carrying one is a producer/consumer
// mismatch: refuse it and leave the order live rather than consume a lie.
#[test]
fn invalid_verifier_sets_are_refused_without_consuming_the_order() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-verify-contract".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let failures = VerifyFailureSet::one(VerifyFailure::Fmt);
    let cases = [
        ("n-pass-set", StageId::Verify, StageVerdict::VerificationPassed, failures),
        ("n-construct-set", StageId::Construct, StageVerdict::VerificationFailed, failures),
        ("n-park-set", StageId::Verify, StageVerdict::Parked, failures),
        ("n-agg-pass-set", StageId::AggregateVerify, StageVerdict::VerificationPassed, failures),
        ("n-agg-review-set", StageId::AggregateReview, StageVerdict::ReviewFinding, failures),
    ];

    for (nonce, stage, verdict, failed_verifiers) in cases {
        let mut record = dispatch_record(nonce, bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
        record.stage = stage;
        record_dispatch(&mut store, &record).unwrap();
        let upload = UploadedEvidence {
            nonce: Nonce(nonce.to_owned()),
            subject: candidate,
            verdict,
            detail: Digest::from_bytes([7; 32]),
            candidate: None,
            findings: None,
            failed_verifiers,
            cost: None,
            calls: None,
        };

        assert!(matches!(
            admit_uploaded(&mut store, &upload).unwrap(),
            AdmitDecision::Refused(IntakeRefusal::InvalidVerifierFailures { .. })
        ));
        assert!(store.lookup_order(nonce).unwrap().is_some(), "`{nonce}` remains live after refusal");
    }
}

// The shared `verify.check` producer emits `failed_verifiers` for a failed
// AggregateVerify the same way it does for a member Verify. Refusing the set
// here stranded the completed order (dispatch 426) instead of routing the
// documented aggregate-verification failure transition.
#[test]
fn a_failing_aggregate_verify_admits_its_typed_set_as_a_bloom_level_failure() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let tree = Digest::from_bytes([30; 32]);
    let mut record = dispatch_record("n-agg-ver", bloom, &WorkpieceId(String::new()), tree, tree);
    record.stage = StageId::AggregateVerify;
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-agg-ver".to_owned()),
        subject: tree,
        verdict: StageVerdict::VerificationFailed,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: None,
        failed_verifiers: VerifyFailureSet::one(VerifyFailure::Clippy),
        cost: None,
        calls: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &upload).unwrap() else {
        panic!("a matching failing AggregateVerify upload is admitted");
    };
    let Fact::AggregateVerifyCompleted { bloom: verified, passed, evidence } = &admission.event.fact else {
        panic!("a failing AggregateVerify admits AggregateVerifyCompleted, got {:?}", admission.event.fact);
    };
    assert_eq!(*verified, bloom);
    assert!(!*passed, "a VerificationFailed verdict fails the aggregate gate");
    assert_eq!(evidence.subject, tree, "the fact retains the intake-validated evidence binding");
    assert!(store.lookup_order("n-agg-ver").unwrap().is_none(), "the admitted order is consumed");
}

// #5098 — a failing AggregateVerify's findings persist on the composition
// workpiece so the reserved Refine can assemble a work order from them. A
// passing verdict clears that row: otherwise a later review-triggered repair
// would still be directed by a compiler diagnostic the fold already cleared.
#[test]
fn aggregate_verify_findings_persist_on_the_composition_and_clear_on_a_pass() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let tree = Digest::from_bytes([30; 32]);
    let findings = "verify.check failed.\n\nerror[E0308]: mismatched types in the fold";
    let upload = |nonce: &str, verdict: StageVerdict, findings: Option<&str>| UploadedEvidence {
        nonce: Nonce(nonce.to_owned()),
        subject: tree,
        verdict,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: findings.map(str::to_owned),
        failed_verifiers: if verdict == StageVerdict::VerificationFailed {
            VerifyFailureSet::one(VerifyFailure::Clippy)
        } else {
            VerifyFailureSet::EMPTY
        },
        cost: None,
        calls: None,
    };

    let mut failing = dispatch_record("n-av-fail", bloom, &WorkpieceId(String::new()), tree, tree);
    failing.stage = StageId::AggregateVerify;
    record_dispatch(&mut store, &failing).unwrap();
    let AdmitDecision::Admitted(_) =
        admit_uploaded(&mut store, &upload("n-av-fail", StageVerdict::VerificationFailed, Some(findings))).unwrap()
    else {
        panic!("the failing aggregate verify admits");
    };
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), WorkpieceId::COMPOSITION).unwrap().as_deref(),
        Some(findings),
        "the composition Refine reads the compiler diagnostic the fold produced",
    );

    let mut passing = dispatch_record("n-av-pass", bloom, &WorkpieceId(String::new()), tree, tree);
    passing.stage = StageId::AggregateVerify;
    record_dispatch(&mut store, &passing).unwrap();
    let AdmitDecision::Admitted(_) =
        admit_uploaded(&mut store, &upload("n-av-pass", StageVerdict::VerificationPassed, None)).unwrap()
    else {
        panic!("the passing aggregate verify admits");
    };
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), WorkpieceId::COMPOSITION).unwrap(),
        None,
        "a passing aggregate verify clears the composition's stale findings",
    );
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
    };
    assert!(matches!(admit_uploaded(&mut store, &delta_fail).unwrap(), AdmitDecision::Admitted(_)));
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), "").unwrap().as_deref(),
        Some(format!("{findings}\n\n## Delta-confirm findings\n\n[wp-a] Still leaking.").as_str()),
        "the delta-confirm's failure appends under its own label, keeping the frozen head",
    );
}

// ADR-0176 — an executor-fault verdict on an AggregateReview order admits the
// fault fact, consumes its order once, and writes no findings at all. The
// findings side effect is the load-bearing half: a fault carries no judgement of
// the fold, so persisting one would hand the next Refine re-entry a defect
// nobody found, and clearing one would lose the frozen set the members were
// already re-opened against.
#[test]
fn an_aggregate_review_executor_fault_admits_its_own_fact_and_touches_no_findings() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let tree = Digest::from_bytes([30; 32]);
    store.record_review_findings(bloom.0.as_bytes(), "", "pillar 2: the members disagree").unwrap();
    let mut record = dispatch_record("n-fault", bloom, &WorkpieceId(String::new()), tree, tree);
    record.stage = StageId::AggregateReview;
    record_dispatch(&mut store, &record).unwrap();

    let fault = UploadedEvidence {
        nonce: Nonce("n-fault".to_owned()),
        subject: tree,
        verdict: StageVerdict::ExecutorFault,
        detail: Digest::from_bytes([9; 32]),
        candidate: None,
        findings: Some("the sandbox refused to start.\nVERDICT: environment".to_owned()),
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
    };
    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &fault).unwrap() else {
        panic!("a matching aggregate fault is admitted");
    };
    let Fact::AggregateReviewExecutorFault { bloom: faulted, evidence } = &admission.event.fact else {
        panic!("an executor-fault verdict admits its own fact, got {:?}", admission.event.fact);
    };
    assert_eq!(*faulted, bloom);
    assert_eq!(evidence.subject, tree, "the fault binds the tree the order displayed");
    assert_eq!(evidence.kind, EvidenceKind::ExecutorFault);
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), "").unwrap().as_deref(),
        Some("pillar 2: the members disagree"),
        "a fault neither appends to the frozen findings nor clears them",
    );

    // Consume-once, like every other admitted result: the order is spent, so a
    // replayed fault refuses rather than buying a second retry.
    assert!(matches!(
        admit_uploaded(&mut store, &fault).unwrap(),
        AdmitDecision::Refused(IntakeRefusal::UnknownNonce(_))
    ));
}

// ADR-0176 ratified the fault lifecycle for the aggregate review alone, so every
// other stage refuses the verdict rather than being handed semantics no decision
// covers. Tripwire: routing it by verdict alone would send a member-stage fault
// into `AttemptCompleted` as an ordinary failure — the flattening this issue
// exists to remove, reintroduced one stage over.
#[test]
fn an_executor_fault_on_any_other_stage_is_refused_and_the_order_stays_live() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let subject = Digest::from_bytes([30; 32]);

    for (nonce, stage) in
        [("n-f-verify", StageId::Verify), ("n-f-construct", StageId::Construct), ("n-f-av", StageId::AggregateVerify)]
    {
        let mut record = dispatch_record(nonce, bloom, &WorkpieceId("wp".to_owned()), subject, subject);
        record.stage = stage;
        record_dispatch(&mut store, &record).unwrap();

        let upload = UploadedEvidence {
            nonce: Nonce(nonce.to_owned()),
            subject,
            verdict: StageVerdict::ExecutorFault,
            detail: Digest::from_bytes([9; 32]),
            candidate: None,
            findings: None,
            failed_verifiers: VerifyFailureSet::EMPTY,
            cost: None,
            calls: None,
        };
        assert!(
            matches!(
                admit_uploaded(&mut store, &upload).unwrap(),
                AdmitDecision::Refused(IntakeRefusal::ExecutorFaultOutOfStage(refused)) if refused == stage
            ),
            "{stage:?} has no ratified fault lifecycle",
        );
        assert!(store.lookup_order(nonce).unwrap().is_some(), "`{nonce}` remains live after refusal");
    }
}

#[test]
fn an_out_of_line_stage_is_refused_and_the_order_stays_live() {
    // A well-formed dispatch only ever carries a dispatched member stage
    // (Construct / Verify / the repair-only Refine / the fold-conflict
    // Reconcile) or a bloom-level aggregate gate; an order at any other
    // stage — the retired member Review included (ADR-0153), and the pre-seal
    // Scope, which is an operator-harness process and never a dispatched lane
    // — is corrupt. It is refused as OutOfLineStage rather than folded into
    // the member's resolution, and (like a digest mismatch) the order is NOT
    // consumed.
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let workpiece = WorkpieceId("wp-off".to_owned());
    let candidate = Digest::from_bytes([5; 32]);
    let mut record = dispatch_record("n-off", bloom, &workpiece, Digest::from_bytes([2; 32]), candidate);
    record.stage = StageId::Scope;
    record_dispatch(&mut store, &record).unwrap();

    let upload = UploadedEvidence {
        nonce: Nonce("n-off".to_owned()),
        subject: candidate,
        verdict: StageVerdict::VerificationPassed,
        detail: Digest::from_bytes([7; 32]),
        candidate: None,
        findings: None,
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
    };
    match admit_uploaded(&mut store, &upload).unwrap() {
        AdmitDecision::Refused(IntakeRefusal::OutOfLineStage(stage)) => {
            assert_eq!(stage, StageId::Scope);
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
        let reference = EvidenceRef {
            name,
            nonce: nonce.clone(),
            artifact_id: 1,
            size_bytes: 10,
            candidate: None,
            findings: None,
            failed_verifiers: VerifyFailureSet::EMPTY,
            cost: None,
            calls: None,
        };

        let decoded = claims.claim_for(&reference).expect("a well-formed attempt name decodes");
        assert_eq!(decoded.nonce, nonce);
        assert_eq!(decoded.subject, subject);
        assert_eq!(decoded.verdict, verdict);
        assert_eq!(decoded.detail, detail);
        assert!(decoded.failed_verifiers.is_empty());
    }

    let nonce = Nonce("dispatch-mask".to_owned());
    let subject = Digest::from_bytes([1; 32]);
    let detail = Digest::from_bytes([2; 32]);
    let failures = [VerifyFailure::Fmt, VerifyFailure::Docs].into_iter().collect::<VerifyFailureSet>();
    let name = NameEvidenceClaims::attempt_artifact_name(
        &nonce,
        &subject,
        StageVerdict::VerificationFailed,
        failures,
        &detail,
    );
    let reference = EvidenceRef {
        name: name.clone(),
        nonce,
        artifact_id: 3,
        size_bytes: 10,
        candidate: None,
        findings: None,
        failed_verifiers: failures,
        cost: None,
        calls: None,
    };
    assert_eq!(claims.claim_for(&reference).expect("typed mask decodes").failed_verifiers, failures);

    // `80` is the eighth identity's mask (ADR-0181), so it is a well-formed token
    // that must now decode rather than be refused. Both projections carry it, so
    // a decode that dropped bit 7 could not hide behind the agreement check below.
    let suppressed = VerifyFailureSet::one(VerifyFailure::Suppress);
    let eighth =
        EvidenceRef { name: name.replacen(".0a.", ".80.", 1), failed_verifiers: suppressed, ..reference.clone() };
    assert_eq!(claims.claim_for(&eighth).expect("the eighth identity's mask decodes").failed_verifiers, suppressed);

    // Tripwire: a malformed mask token must be refused by the name decode itself
    // rather than incidentally by the body/name agreement check. Each case pairs
    // the token with the set a lax decode would read out of it — `0A` is the
    // reference's own mask in uppercase, while `gg` and the one-character `0`
    // would fall open to the empty set — so agreement holds and only the decode
    // can reject. Leave the body at its original set and every case here passes
    // on the disagreement instead, proving nothing about the token.
    for (malformed_mask, lax_reading) in
        [("0A", failures), ("gg", VerifyFailureSet::EMPTY), ("0", VerifyFailureSet::EMPTY)]
    {
        let malformed = EvidenceRef {
            name: name.replacen(".0a.", &format!(".{malformed_mask}."), 1),
            failed_verifiers: lax_reading,
            ..reference.clone()
        };
        assert!(claims.claim_for(&malformed).is_none(), "mask `{malformed_mask}` must not become an upload");
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
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
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
    let missing_kit = DispatchError::Submit(ExecutorPortError::Local(LocalExecutorError::MissingKit(
        "lane host is missing kit tools on PATH `(unset)`:\n- `jscpd` — npm install -g jscpd".to_owned(),
    )));
    let store = DispatchError::Store(rusqlite::Error::QueryReturnedNoRows);

    for error in [transport, decode, pagination, no_run, local, missing_kit, store] {
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
        failed_verifiers: if verdict == StageVerdict::VerificationFailed {
            VerifyFailureSet::one(VerifyFailure::Clippy)
        } else {
            VerifyFailureSet::EMPTY
        },
        cost: None,
        calls: None,
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

/// The study lane, end to end (#4679): a completed attempt that reported usage
/// writes exactly one priced `study_index` row bound to the digest its order
/// displayed.
///
/// The three shapes below are the ones that decide whether the ledger can be
/// trusted, so they are asserted together against one live cycle rather than in
/// isolation: what a measured attempt records, what an unmeasured one records,
/// and what a record that names the wrong digest records.
#[test]
fn a_measured_attempt_writes_one_priced_study_row_and_an_unmeasured_one_writes_none() {
    use aether_bloomery::ConfigKind as _;
    use aether_bloomery::{Harness, PriceRates, PriceTable, ReasoningEffort, ResolvedModel, StudyCost, StudyRecord};
    use aether_data::Kind as _;
    use aether_data::wire::{from_bytes, to_vec};

    use crate::artifacts::{ArtifactsCapabilityState, GetResult};

    let workpiece = WorkpieceId("wp-study".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let (_snapshot, bloom) = sealed_snapshot(&workpiece, scope_revision);
    let candidate = Digest::from_bytes([5; 32]);

    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let mut store = store();
    let artifacts_dir = tempfile::tempdir().unwrap();
    let mut artifacts = ArtifactsCapabilityState::open(artifacts_dir.path()).unwrap();

    // Seal a table pricing exactly the model this order runs, the way `POST
    // /configs` would author it.
    let table = PriceTable {
        rows: BTreeMap::from([(
            "muse-spark-1.2-contributor".to_owned(),
            PriceRates { input: 1_000_000, cache_read: 100_000, output: 4_000_000, ..PriceRates::default() },
        )]),
    };
    let bytes = to_vec(&table).unwrap();
    store.record_config(table.address().as_bytes(), PriceTable::NAME, &bytes).unwrap();
    let mut configs = ConfigRegistry::default();
    configs.insert::<PriceTable>(table.address());

    let mut record = dispatch_record("n-study", bloom, &workpiece, scope_revision, candidate);
    record.configs = configs;
    record.transformation.model = Some(ResolvedModel {
        harness: Harness::Muse,
        model: "muse-spark-1.2-contributor".to_owned(),
        effort: ReasoningEffort::Medium,
    });
    let handle = dispatch_and_record(&shell, &mut store, &record, NOW_UNIX_MILLIS).unwrap();

    let run_id = fake.seed_run("n-study", RunStatus::Completed, Some(RunConclusion::Success));
    fake.seed_run_artifacts(run_id, vec![Artifact { id: 1, name: "evidence-n-study".to_owned(), size_bytes: 10 }]);

    // 2M input, 10M cache-read, 500k output → 2.00 + 1.00 + 2.00 = $5.00.
    let cost = StudyCost {
        input_tokens: 2_000_000,
        cache_read_tokens: 10_000_000,
        output_tokens: 500_000,
        ..StudyCost::default()
    };
    let mut claims = HashMap::new();
    claims.insert(
        "n-study".to_owned(),
        UploadedEvidence {
            nonce: Nonce("n-study".to_owned()),
            subject: candidate,
            verdict: StageVerdict::VerificationPassed,
            detail: Digest::from_bytes([7; 32]),
            candidate: None,
            findings: None,
            failed_verifiers: VerifyFailureSet::EMPTY,
            cost: Some(cost),
            calls: None,
        },
    );
    let claims = SeededClaims(claims);
    let mut sink = Collector::default();

    let report = run_intake_cycle(&mut store, &shell, &[handle], &claims, Some(&mut artifacts), &mut sink).unwrap();

    assert_eq!(report.studied, 1, "the measured attempt recorded its cost");
    assert_eq!(report.admitted, 1, "and the verdict admitted on the same pass");
    assert!(
        sink_has_study_evidence(&sink),
        "the cycle journals the study artifact so a calibration read can resolve it"
    );

    // The row is keyed by the digest the order *displayed*, not by anything the
    // upload claimed for itself.
    let indexed = store
        .lookup_study(bloom.0.as_bytes(), candidate.as_bytes())
        .unwrap()
        .expect("one study row bound to the displayed digest");

    // The dollar column is ours, computed from the sealed table — the record the
    // harness uploaded carried no price at all.
    let GetResult::Ok { bytes, .. } = artifacts.get(indexed) else {
        panic!("the study artifact the index points at is retrievable");
    };
    let stored: StudyRecord = from_bytes(&bytes).unwrap();
    assert_eq!(stored.cost.cost_micro_usd, 5_000_000, "priced from the sealed table, not from the runner");
    assert_eq!(stored.cost.input_tokens, 2_000_000, "and the measured tokens survive alongside it");

    // An unmeasured attempt — the Actions lane, or a harness that reported no
    // usage. It must record *nothing* rather than a row of zeroes, which would
    // be indistinguishable from a free attempt and would deflate every average
    // taken over the ledger.
    let unmeasured = second_attempt(&fake, &shell, &mut store, bloom, &workpiece, scope_revision, "n-bare", None);
    let report =
        run_intake_cycle(&mut store, &shell, &[unmeasured.0], &unmeasured.1, Some(&mut artifacts), &mut sink).unwrap();

    assert_eq!(report.studied, 0, "an unmeasured attempt writes no study row");
    assert_eq!(report.admitted, 1, "but it still admits normally — the study lane grades, it does not gate");

    // A record naming a digest its order never displayed. The binding check is
    // the whole trust boundary: a worker that could grade an arbitrary digest
    // could attribute its costs to any attempt in the journal.
    let lying = second_attempt(
        &fake,
        &shell,
        &mut store,
        bloom,
        &workpiece,
        scope_revision,
        "n-liar",
        Some((Digest::from_bytes([0xEE; 32]), cost)),
    );
    let before = store.lookup_study(bloom.0.as_bytes(), Digest::from_bytes([0xEE; 32]).as_bytes()).unwrap();
    let report = run_intake_cycle(&mut store, &shell, &[lying.0], &lying.1, Some(&mut artifacts), &mut sink).unwrap();

    assert_eq!(report.studied, 0, "a record grading a digest the order never displayed is refused");
    assert_eq!(before, None);
    assert_eq!(
        store.lookup_study(bloom.0.as_bytes(), Digest::from_bytes([0xEE; 32]).as_bytes()).unwrap(),
        None,
        "and nothing was written under the digest it claimed",
    );
}

/// Dispatch one more attempt on the same bloom and seed its completed run,
/// returning the handle plus the claims that upload `cost` for it. `subject`
/// defaults to the order's own displayed digest; passing an explicit one is how
/// the mis-binding case is built.
#[expect(clippy::too_many_arguments, reason = "a test fixture threading one dispatch's full context")]
fn second_attempt(
    fake: &FakeGithub,
    shell: &ExecutorShell,
    store: &mut dyn StoreBackend,
    bloom: BloomId,
    workpiece: &WorkpieceId,
    scope_revision: Digest,
    nonce: &str,
    claimed: Option<(Digest, aether_bloomery::StudyCost)>,
) -> (WorkHandle, SeededClaims) {
    let candidate = Digest::from_bytes([5; 32]);
    let record = dispatch_record(nonce, bloom, workpiece, scope_revision, candidate);
    let handle = dispatch_and_record(shell, store, &record, NOW_UNIX_MILLIS).unwrap();
    let run_id = fake.seed_run(nonce, RunStatus::Completed, Some(RunConclusion::Success));
    fake.seed_run_artifacts(run_id, vec![Artifact { id: 1, name: format!("evidence-{nonce}"), size_bytes: 10 }]);

    let (subject, cost) = claimed.map_or((candidate, None), |(subject, cost)| (subject, Some(cost)));
    let mut claims = HashMap::new();
    claims.insert(
        nonce.to_owned(),
        UploadedEvidence {
            nonce: Nonce(nonce.to_owned()),
            subject,
            verdict: StageVerdict::VerificationPassed,
            detail: Digest::from_bytes([7; 32]),
            candidate: None,
            findings: None,
            failed_verifiers: VerifyFailureSet::EMPTY,
            cost,
            calls: None,
        },
    );
    (handle, SeededClaims(claims))
}

/// The finding from bloom `10a1228c` (#4959): a coverage gap that fails no
/// mechanical gate, naming one symbol and the file it lives in — a judge's
/// prose, which post-ADR-0191 lands on the composition's channel.
const REPAIR_FINDING: &str = "[wp-golden] `representative()` in \
                              `crates/aether-bloomery/tests/golden_decisions/main.rs` does not reach every effect \
                              family, so the pinned bytes freeze less than the graph.";

/// The dodge the refine lane returned, twice: a real edit, in the file the
/// finding named, changing nothing the finding is about.
const DODGE_DIFF: &str = "--- a/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                          +++ b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                          @@ -530,7 +530,7 @@ fn the_decisions_graph_is_wire_frozen() {\n\
                          -    let encoded = to_vec(&decisions).unwrap();\n\
                          +    let encoded = to_vec(&decisions).expect(\"a fixture value wire-encodes\");\n";

/// A repair that actually reaches the named symbol.
const REAL_REPAIR_DIFF: &str = "--- a/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                                +++ b/crates/aether-bloomery/tests/golden_decisions/main.rs\n\
                                @@ -240,6 +240,7 @@ fn representative() -> Decisions {\n\
                                +            record_operator_repair(bloom, workpiece.clone()),\n";

/// A bloom whose composition sits mid-weave-repair on its `attempt`th lap, with
/// the refused weave to repair from (ADR-0191 §5) — where a weave repair's
/// result lands.
fn composition_reweaving(
    member: &WorkpieceId,
    scope_revision: Digest,
    weave: Digest,
    attempt: u32,
) -> (Snapshot, BloomId) {
    let (mut snapshot, bloom) = sealed_via_reducer(member, scope_revision);
    let record = snapshot.blooms.get_mut(&bloom).expect("the sealed bloom");
    record.progress.insert(
        WorkpieceId::composition(),
        aether_bloomery::StageProgress {
            stage: StageId::Refine,
            attempts: attempt,
            candidate: Some(aether_bloomery::CandidateRef { tree: weave, checkout: weave }),
            repair_rolls: 0,
            seen_verify_failures: VerifyFailureSet::EMPTY,
            fold_checkpoint: None,
            fold_conflict_evidence: None,
        },
    );
    (snapshot, bloom)
}

/// The outstanding `Refine` order a weave repair answers.
fn weave_repair_record(nonce: &str, bloom: BloomId, scope_revision: Digest, weave: Digest) -> DispatchRecord {
    let mut record = dispatch_record(nonce, bloom, &WorkpieceId::composition(), scope_revision, weave);
    record.stage = StageId::Refine;
    record
}

/// Stage a repair lap: the finding that dispatched it, the diff it captured, and
/// its outstanding order. The finding is filed bloom-scoped, which is where a
/// failing composition review freezes it and what the weave repair's prompt reads.
fn stage_repair_lap(store: &mut SqliteStore, record: &DispatchRecord, finding: &str, diff: Option<&str>) {
    store.record_review_findings(record.bloom.0.as_bytes(), "", finding).unwrap();
    if let Some(diff) = diff {
        store.record_capture_diff(&record.nonce.0, diff).unwrap();
    }
    record_dispatch(store, record).unwrap();
}

/// The passing repair-lap upload a lane returns: a conclusive verdict plus the
/// candidate it captured.
fn repair_upload(nonce: &str, subject: Digest) -> UploadedEvidence {
    UploadedEvidence {
        nonce: Nonce(nonce.to_owned()),
        subject,
        verdict: StageVerdict::VerificationPassed,
        detail: Digest::from_bytes([7; 32]),
        candidate: Some(aether_bloomery::CandidateRef {
            tree: Digest::from_bytes([8; 32]),
            checkout: Digest::from_bytes([9; 32]),
        }),
        findings: None,
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
        calls: None,
    }
}

// Tripwire: the incident (#4959, bloom `10a1228c`), in its post-ADR-0191 shape.
// A weave repair that edits the file its finding named while leaving the named
// symbol alone must be bounced by the host before anything is re-judged — and the
// bounce must be a *failing* lap, so it spends the budget a refused lap spends
// instead of opening a second loop. If this ever admits `passed: true`, the dodge
// buys a whole-bloom Opus review round again.
#[test]
fn a_weave_repair_that_dodges_its_finding_bounces_without_a_re_judge_and_spends_a_retry() {
    let mut store = store();
    let member = WorkpieceId("wp-golden".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let weave = Digest::from_bytes([5; 32]);
    let (snapshot, bloom) = composition_reweaving(&member, scope_revision, weave, 1);
    let record = weave_repair_record("n-dodge", bloom, scope_revision, weave);
    stage_repair_lap(&mut store, &record, REPAIR_FINDING, Some(DODGE_DIFF));

    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &repair_upload("n-dodge", weave)).unwrap()
    else {
        panic!("the lap admits — as a failing one");
    };
    let Fact::AttemptCompleted { workpiece, stage, passed, evidence, .. } = &admission.event.fact else {
        panic!("a Refine result admits AttemptCompleted");
    };
    assert!(workpiece.is_composition());
    assert_eq!(*stage, StageId::Refine);
    assert!(!*passed, "the lane's conclusive verdict is downgraded: the repair changed nothing the finding named");
    assert_eq!(
        evidence.kind,
        EvidenceKind::RepairTriage,
        "the bounce is filed under its own kind so the study layer can count dodges",
    );

    // The reducer charges the ordinary weave-repair retry and re-weaves — the
    // composite gate run is never dispatched, so no aggregate review behind it.
    let decisions = reduce(&snapshot, &admission.event, &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(
        matches!(decisions.outcome, Outcome::CompositionRewoven { refused_at: StageId::Refine, attempt: 2, .. }),
        "the bounce spends a weave-repair retry, got {:?}",
        decisions.outcome,
    );
    assert!(
        !decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
        "nothing is re-judged: the dodging weave never reaches the composite gate run",
    );

    // The journal carries the verdict, and the admitted bytes replay to it.
    assert!(
        decisions.effects.iter().any(
            |effect| matches!(effect, Decision::RecordEvidence { evidence, .. } if evidence.kind == EvidenceKind::RepairTriage),
        ),
        "the triage verdict is journaled with the dispatch",
    );
    assert_eq!(
        from_bytes::<Event>(&admission.admit.event).unwrap(),
        admission.event,
        "the admitted event round-trips the wire, so the bounce replays",
    );

    // The dodging capture is discarded and the finding is re-threaded, with a
    // section naming what the bounced lap missed — on the composition's own row,
    // leaving the frozen set a delta-confirm is framed against untouched.
    let next = snapshot.apply(&admission.event, &decisions, &ResolvedConfigs::default());
    let cursor = next.blooms.get(&bloom).unwrap().progress.get(&WorkpieceId::composition()).copied().unwrap();
    assert_eq!(cursor.stage, StageId::Refine);
    assert_eq!(cursor.candidate.map(|current| current.tree), Some(weave), "the dodge's capture is not adopted");
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), "").unwrap().as_deref(),
        Some(REPAIR_FINDING),
        "the frozen aggregate set is left verbatim",
    );
    let threaded = store.lookup_review_findings(bloom.0.as_bytes(), &WorkpieceId::composition().0).unwrap().unwrap();
    assert!(threaded.starts_with(REPAIR_FINDING), "the original finding is re-threaded verbatim");
    assert!(threaded.contains("## Repair triage"), "the next lap is told what the last one missed");
    assert!(threaded.contains("`representative`"), "the note names the symbol that went untouched");
}

#[test]
fn a_weave_repair_that_touches_its_finding_passes_to_the_re_judge() {
    let mut store = store();
    let member = WorkpieceId("wp-golden".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let weave = Digest::from_bytes([5; 32]);
    let (snapshot, bloom) = composition_reweaving(&member, scope_revision, weave, 1);
    let record = weave_repair_record("n-real", bloom, scope_revision, weave);
    stage_repair_lap(&mut store, &record, REPAIR_FINDING, Some(REAL_REPAIR_DIFF));

    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &repair_upload("n-real", weave)).unwrap()
    else {
        panic!("the lap admits");
    };
    let Fact::AttemptCompleted { passed, evidence, .. } = &admission.event.fact else {
        panic!("a Refine result admits AttemptCompleted");
    };
    assert!(*passed, "a repair that reaches the named symbol keeps its passing verdict");
    assert_eq!(evidence.kind, EvidenceKind::VerificationResult, "an addressed lap is filed as the verdict it is");

    let decisions = reduce(&snapshot, &admission.event, &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(
        matches!(decisions.outcome, Outcome::CompositionRepaired { .. }),
        "the repair hands the re-woven tree to the composite gate run, got {:?}",
        decisions.outcome,
    );
    assert!(
        decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
        "the re-judge is dispatched",
    );
}

#[test]
fn a_finding_that_names_nothing_never_bounces_a_lap() {
    let mut store = store();
    let member = WorkpieceId("wp-vague".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let weave = Digest::from_bytes([5; 32]);
    let (_snapshot, bloom) = composition_reweaving(&member, scope_revision, weave, 1);
    let record = weave_repair_record("n-vague", bloom, scope_revision, weave);
    stage_repair_lap(
        &mut store,
        &record,
        "The retry loop feels wrong and the naming could be clearer throughout.",
        Some(DODGE_DIFF),
    );

    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &repair_upload("n-vague", weave)).unwrap()
    else {
        panic!("the lap admits");
    };
    let Fact::AttemptCompleted { passed, .. } = &admission.event.fact else {
        panic!("a Refine result admits AttemptCompleted");
    };
    assert!(*passed, "a finding that names no symbol has no claim to test, so the lap passes");
    assert_eq!(
        store.lookup_review_findings(bloom.0.as_bytes(), &WorkpieceId::composition().0).unwrap(),
        None,
        "a passing lap threads no note",
    );
}

// Tripwire: a lane that dodges repeatedly must reach the operator rather than
// ping-ponging. The bounce is an ordinary failing lap, so the sealed `Refine`
// budget is what ends it — a triage that spent no budget would loop forever, and
// #4957's manager override is the escape hatch from the wedge, not a second loop.
#[test]
fn repeated_dodges_exhaust_the_repair_budget_and_wedge_the_composition() {
    let mut store = store();
    let member = WorkpieceId("wp-golden".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let weave = Digest::from_bytes([5; 32]);
    let budget = StageCatalog::line().retry_budget_of(StageId::Refine).unwrap();
    let (snapshot, bloom) = composition_reweaving(&member, scope_revision, weave, budget);
    let record = weave_repair_record("n-last", bloom, scope_revision, weave);
    stage_repair_lap(&mut store, &record, REPAIR_FINDING, Some(DODGE_DIFF));

    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &repair_upload("n-last", weave)).unwrap()
    else {
        panic!("the lap admits");
    };
    let decisions = reduce(&snapshot, &admission.event, &ResolvedConfigs::default(), &SpendWindow::default());
    assert!(
        matches!(decisions.outcome, Outcome::CompositionWedged { refused_at: StageId::Refine, .. }),
        "the last dodge exhausts the budget and stops the composition, got {:?}",
        decisions.outcome,
    );
    let next = snapshot.apply(&admission.event, &decisions, &ResolvedConfigs::default());
    assert!(
        next.blooms.get(&bloom).unwrap().wedged.contains_key(&WorkpieceId::composition()),
        "the operator sees a wedged composition",
    );
}

// Tripwire: a *member's* repair lap is never triaged. Post-ADR-0191 an aggregate
// refusal does not re-open a member, so every finding reaching a member's Refine
// is mechanical gate output — a compiler diagnostic backticks the symptom's types
// and locations, not the thing a fix has to change, so triaging on it would bounce
// honest laps on the loop's highest-volume path. ADR-0178's repeated-verifier
// accounting is the member side's own unrepaired-candidate detector.
#[test]
fn a_members_repair_lap_is_not_triaged_against_mechanical_diagnostics() {
    let mut store = store();
    let workpiece = WorkpieceId("wp-member".to_owned());
    let scope_revision = Digest::from_bytes([2; 32]);
    let candidate = Digest::from_bytes([5; 32]);
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let mut record = dispatch_record("n-member", bloom, &workpiece, scope_revision, candidate);
    record.stage = StageId::Refine;
    // A rustc diagnostic: it backticks `u32` and `usize` and names the file the
    // symptom surfaced in, none of which a fix elsewhere has to mention.
    store
        .record_review_findings(
            bloom.0.as_bytes(),
            &workpiece.0,
            "verify.check failed.\n\nerror[E0308]: mismatched types\n  --> crates/mock/src/lib.rs:7:20\n   \
             |                --- ^^^^^^^ expected `u32`, found `usize`",
        )
        .unwrap();
    store.record_capture_diff("n-member", DODGE_DIFF).unwrap();
    record_dispatch(&mut store, &record).unwrap();

    let AdmitDecision::Admitted(admission) = admit_uploaded(&mut store, &repair_upload("n-member", candidate)).unwrap()
    else {
        panic!("the lap admits");
    };
    let Fact::AttemptCompleted { passed, evidence, .. } = &admission.event.fact else {
        panic!("a Refine result admits AttemptCompleted");
    };
    assert!(*passed, "a member's repair lap keeps its verdict; the triage does not read mechanical findings");
    assert_eq!(evidence.kind, EvidenceKind::VerificationResult);
}

// #4961 — the advisory half of the classification, at the trust boundary. The
// review lane already decided the verdict from the classes the critic stated, so
// what arrives here is a *pass* whose prose still carries judgment findings; the
// broker re-kinds it so the reducer files them on the composition's channel on
// the way to the landing.
//
// Two tripwires. Re-kinding a pass that carries no advisories would file a
// finding on every clean review, filling the channel an operator reads with rows
// no verdict raised. And *not* re-kinding an advisory-carrying pass is the
// silent-loss direction — the bloom resolves, the findings evaporate, and the
// reviewer's judgment call was written for nobody.
#[test]
fn a_passing_review_carrying_advisories_is_kinded_as_one() {
    let mut store = store();
    let bloom = BloomId(Digest::from_bytes([1; 32]));
    let tree = Digest::from_bytes([30; 32]);
    let admit_pass = |store: &mut SqliteStore, nonce: &str, findings: Option<&str>| {
        let mut record = dispatch_record(nonce, bloom, &WorkpieceId(String::new()), tree, tree);
        record.stage = StageId::AggregateReview;
        record_dispatch(store, &record).unwrap();
        let upload = UploadedEvidence {
            nonce: Nonce(nonce.to_owned()),
            subject: tree,
            verdict: StageVerdict::Approved,
            detail: Digest::from_bytes([9; 32]),
            candidate: None,
            findings: findings.map(str::to_owned),
            failed_verifiers: VerifyFailureSet::EMPTY,
            cost: None,
            calls: None,
        };
        let AdmitDecision::Admitted(admission) = admit_uploaded(store, &upload).unwrap() else {
            panic!("a matching passing aggregate verdict is admitted");
        };
        let Fact::AggregateReviewCompleted { passed, evidence, .. } = admission.event.fact else {
            panic!("an AggregateReview order admits AggregateReviewCompleted");
        };
        assert!(passed, "the lane's own verdict stands; the broker only re-kinds it");
        evidence.kind
    };

    assert_eq!(
        admit_pass(&mut store, "n-adv", Some("- JUDGMENT — src/reduce.rs: `weave` would read better here.")),
        EvidenceKind::ReviewAdvisory,
        "a pass whose prose records judgment findings is an advisory pass",
    );
    // `Approval` is what `normalize_stage_result` gives an approving verdict;
    // these two are the untouched shape, and the point is that the re-kinding
    // never reaches them.
    assert_eq!(
        admit_pass(&mut store, "n-clean", Some("all five pillars clean; the seam preserves both intents.")),
        EvidenceKind::Approval,
        "a pass whose prose classifies nothing files nothing",
    );
    assert_eq!(
        admit_pass(&mut store, "n-blocking", Some("- JUDGMENT (critical: it drops the budget) — src/reduce.rs")),
        EvidenceKind::Approval,
        "a blocking class on a passing verdict is not an advisory — the lane would have reported a fail",
    );
}
