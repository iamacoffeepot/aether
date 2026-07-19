//! The drain → submit → record and pull → admit core of the executor dispatch
//! driver, over a real `SqliteStore` and a fake-GitHub-backed `ExecutorShell` —
//! the network side the running capability drives, without the mail harness.
//! `init` / the timer / the ctx send are the thin glue the chassis-boot test and
//! compilation cover; this pins the loop that actually dispatches and admits.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_bloomery::{
    AggregateReviewPayload, BloomId, DispatchPayload, EvidenceRef, ExecutionStatus, ExecutorBackend, Fact, Nonce,
    StageCatalog, StageId, TOPIC_AGGREGATE_REVIEW, Transformation, WorkHandle, WorkOrder, WorkpieceId,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{
    ActionsExecutor, Artifact, ExecutorError, GithubError, RunConclusion, RunStatus, StageVerdict,
};
use aether_data::wire::{from_bytes, to_vec};

use super::{
    BACKOFF_CAP, CandidatePush, NameEvidenceClaims, TOPIC_DISPATCH, TrackedHandle, backoff_delay, candidate_ref_name,
    drain_and_dispatch, drain_and_dispatch_aggregate, is_stale, next_backoff, pull_and_admit, push_admitted_candidates,
    seed_tracked, select_stale_handles,
};
use crate::bloomery::intake::{Admission, attempt_artifact_name};
use crate::bloomery::local_executor::testing::FixedRunner;
use crate::bloomery::{ExecutorPortError, ExecutorShell, LocalExecutor, RoutingExecutor, RunLifecycle};
use crate::store::{SqliteStore, StoreBackend};

// A capturing executor backend: it records every submitted `WorkOrder` so a test
// can assert exactly what `drain_and_dispatch` built — the advisory description it
// threaded onto the construct transformation (#3595) in particular. Only `submit`
// is driven by the drain; the other port methods are inert stubs.
#[derive(Default)]
struct CapturingBackend {
    orders: Mutex<Vec<WorkOrder>>,
}

impl CapturingBackend {
    fn orders(&self) -> Vec<WorkOrder> {
        self.orders.lock().unwrap().clone()
    }
}

impl ExecutorBackend for CapturingBackend {
    type Error = ExecutorPortError;

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        self.orders.lock().unwrap().push(order.clone());
        Ok(WorkHandle::new(order.nonce.clone()))
    }

    fn inspect(&self, _handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        Ok(ExecutionStatus::Unknown)
    }

    fn cancel(&self, _handle: &WorkHandle) -> Result<(), Self::Error> {
        Ok(())
    }

    fn stream_evidence(&self, _handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        Ok(Vec::new())
    }
}

// The inert candidate-push stub for tests that exercise the pull/admit loop
// without asserting on the push side.
struct NopPush;

impl CandidatePush for NopPush {
    fn push(&self, _commit_hex: &str, _target_ref: &str) -> Result<(), String> {
        Ok(())
    }
}

// A recording push seam, for asserting exactly which (commit, ref) pairs the
// admitted-candidate push issued.
#[derive(Default)]
struct RecordingPush {
    pushed: Mutex<Vec<(String, String)>>,
}

impl CandidatePush for RecordingPush {
    fn push(&self, commit_hex: &str, target_ref: &str) -> Result<(), String> {
        self.pushed.lock().unwrap().push((commit_hex.to_owned(), target_ref.to_owned()));
        Ok(())
    }
}

const WORKFLOW: &str = "bloomery-transform.yml";
const PINNED_REF: &str = "refs/heads/main";

fn shell(fake: FakeGithub) -> ExecutorShell {
    // The dispatched orders check out `digest(0xC0)`; seed its correspondence so
    // `submit` resolves the subject (the fake's store is shared across clones).
    fake.seed_git_object(&digest(0xC0));
    ExecutorShell::new(Arc::new(ActionsExecutor::new(fake.clone(), Arc::new(fake), WORKFLOW, PINNED_REF)))
}

/// A backend whose `submit` always refuses with a fixed HTTP status, for
/// exercising `drain_and_dispatch`'s permanent-vs-transient branch. The fake
/// GitHub client carries no fault-injection hook, so this test double lives
/// host-side, mirroring `ActionsExecutor` at the same `ExecutorBackend` seam.
struct FailingExecutor {
    status: u16,
}

impl ExecutorBackend for FailingExecutor {
    type Error = ExecutorError;

    fn submit(&self, _order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        Err(ExecutorError::Github(GithubError::Status { status: self.status, body: "refused".to_owned() }))
    }

    fn inspect(&self, _handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        Ok(ExecutionStatus::Unknown)
    }

    fn cancel(&self, _handle: &WorkHandle) -> Result<(), Self::Error> {
        Ok(())
    }

    fn stream_evidence(&self, _handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        Ok(Vec::new())
    }
}

fn failing_shell(status: u16) -> ExecutorShell {
    ExecutorShell::new(Arc::new(FailingExecutor { status }))
}

fn digest(seed: u8) -> aether_bloomery::Digest {
    aether_bloomery::Digest::from_bytes([seed; 32])
}

// Wrap freshly-dispatched handles into `TrackedHandle`s the way `on_dispatch_tick`
// does at its `state.tracked.extend` site, so a test can drive `pull_and_admit`
// directly without the mail harness.
fn track(handles: Vec<WorkHandle>) -> Vec<TrackedHandle> {
    let now = Instant::now();
    handles.into_iter().map(|handle| TrackedHandle::new(handle, now)).collect()
}

// Enqueue one per-member Construct dispatch on the dispatch topic (the bytes the
// reducer's `DispatchAttempt` projection would enqueue), returning its outbox
// sequence and the subject digest the attempt runs against.
fn enqueue_construct_dispatch(store: &mut SqliteStore, bloom: BloomId, workpiece: &str, subject: u8) -> (u64, u8) {
    let payload = DispatchPayload {
        bloom: bloom.0,
        workpiece: WorkpieceId(workpiece.to_owned()),
        stage: StageId::Construct,
        transformation: Transformation::for_member_stage(StageId::Construct, digest(subject), digest(0xC0)),
        scope_revision: digest(subject),
        candidate: None,
    };
    let sequence = store.enqueue_outbox(TOPIC_DISPATCH, &to_vec(&payload).unwrap()).unwrap();
    (sequence, subject)
}

// ADR-0153 — the aggregate-review topic drains into a bloom-level order: the
// review.critic transformation submits with a task composed from every
// member's persisted work order, and the intake record carries the
// AggregateReview stage with no member axis (the empty workpiece) and the
// integrated tree as its displayed digest. Catches the aggregate dispatch
// never leaving the outbox, and a record mis-keyed to a member.
#[test]
fn drain_and_dispatch_aggregate_submits_a_bloom_level_review_order() {
    let mut store = SqliteStore::open(":memory:").unwrap();
    let backend = Arc::new(CapturingBackend::default());
    let shell = ExecutorShell::new(Arc::clone(&backend));
    let bloom = BloomId(digest(1));
    store.record_dispatch_description(bloom.0.as_bytes(), "wp-a", "build the widget").unwrap();
    store.record_dispatch_description(bloom.0.as_bytes(), "wp-b", "wire the widget").unwrap();
    let payload = AggregateReviewPayload {
        bloom: bloom.0,
        transformation: Transformation::for_aggregate_review(digest(30), digest(40)),
        roll: 1,
    };
    let sequence = store.enqueue_outbox(TOPIC_AGGREGATE_REVIEW, &to_vec(&payload).unwrap()).unwrap();

    let (handles, ack_through, _transient) = drain_and_dispatch_aggregate(&mut store, &shell).unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(ack_through, Some(sequence));

    let orders = backend.orders();
    let order = &orders[0];
    assert_eq!(order.transformation.command, "review.critic");
    assert_eq!(order.transformation.inputs[0], digest(30), "the evidence binds the integrated tree");
    assert_eq!(order.transformation.checkout, digest(40), "the critic checks out the integrated head");
    let description = order.transformation.description.as_deref().unwrap();
    assert!(description.contains("## Task — wp-a\n\nbuild the widget"), "every member's order composes in");
    assert!(description.contains("## Task — wp-b\n\nwire the widget"));

    let stored = store.lookup_order(&order.nonce.0).unwrap().expect("the bloom-level order is recorded");
    assert_eq!(stored.workpiece, "", "a bloom-level order has no member axis");
    assert_eq!(stored.displayed_digest, digest(30).as_bytes().to_vec(), "the verdict must bind the integrated tree");
    assert_eq!(stored.bloom, bloom.0.as_bytes().to_vec());
}

#[test]
fn drain_and_dispatch_submits_each_dispatch_and_records_its_order() {
    let mut store = SqliteStore::open(":memory:").unwrap();
    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let bloom = BloomId(digest(1));
    let (sequence, _subject) = enqueue_construct_dispatch(&mut store, bloom, "wp-line", 5);

    let (handles, ack_through, _transient_failure) = drain_and_dispatch(&mut store, &shell).unwrap();

    // One dispatch submitted, its order recorded, and the ack prefix covers it.
    assert_eq!(handles.len(), 1);
    assert_eq!(ack_through, Some(sequence));
    let nonce = format!("dispatch-{sequence}");
    assert_eq!(fake.dispatched_nonces(), vec![nonce.clone()], "the work order reached the executor");
    let order = store.lookup_order(&nonce).unwrap().expect("the intake registry recorded the order");
    assert_eq!(order.workpiece, "wp-line");

    // Acking the prefix means the entry does not re-drain.
    store.ack_outbox(Some(TOPIC_DISPATCH), sequence).unwrap();
    assert!(store.drain_outbox(Some(TOPIC_DISPATCH)).unwrap().is_empty(), "the acked dispatch does not re-drain");
}

#[test]
fn drain_threads_the_persisted_description_onto_the_construct_order() {
    // The #3595 seal → dispatch seam over a real store + executor shell: the
    // description the coordinator persisted at seal (modeled by the store write
    // seal_draft performs) is looked up by (bloom, workpiece) and threaded onto
    // the submitted construct order's transformation, so the construct lane can
    // name it in its `## Task` prompt.
    let mut store = SqliteStore::open(":memory:").unwrap();
    let backend = Arc::new(CapturingBackend::default());
    let shell = ExecutorShell::new(Arc::clone(&backend));
    let bloom = BloomId(digest(1));
    store.record_dispatch_description(bloom.0.as_bytes(), "wp-line", "thread the work order into the prompt").unwrap();
    enqueue_construct_dispatch(&mut store, bloom, "wp-line", 5);

    drain_and_dispatch(&mut store, &shell).unwrap();

    let orders = backend.orders();
    assert_eq!(orders.len(), 1, "the construct dispatch submitted");
    assert_eq!(
        orders[0].transformation.description.as_deref(),
        Some("thread the work order into the prompt"),
        "the persisted description reached the submitted construct order",
    );
}

#[test]
fn drain_leaves_the_description_none_and_still_dispatches_when_none_persisted() {
    // The fail-legible path: a member with no persisted description dispatches
    // subject-only (description `None`) rather than being dropped — the #3596
    // completion gate then catches an empty-work run, but the dispatch itself
    // must never silently vanish.
    let mut store = SqliteStore::open(":memory:").unwrap();
    let backend = Arc::new(CapturingBackend::default());
    let shell = ExecutorShell::new(Arc::clone(&backend));
    let bloom = BloomId(digest(1));
    enqueue_construct_dispatch(&mut store, bloom, "wp-line", 5);

    let (handles, _, _) = drain_and_dispatch(&mut store, &shell).unwrap();

    assert_eq!(handles.len(), 1, "a member with no description still dispatches, never dropped");
    let orders = backend.orders();
    assert_eq!(orders.len(), 1);
    assert!(
        orders[0].transformation.description.is_none(),
        "no persisted description leaves the transformation None — a legible subject-only run",
    );
}

#[test]
fn drain_stops_the_ack_prefix_at_a_missing_subject_entry() {
    // Tripwire: a dispatch entry carrying no subject input stops the ack prefix at
    // the last success (a `break`, like the decode/submit-failure paths) rather than
    // being skipped and acked past by a later success in the same drain — a swallowed
    // entry would never re-drain.
    let mut store = SqliteStore::open(":memory:").unwrap();
    let shell = shell(FakeGithub::new());
    let bloom = BloomId(digest(1));

    // A well-formed dispatch, then a subject-less one, then another well-formed one.
    let (first, _) = enqueue_construct_dispatch(&mut store, bloom, "wp-a", 5);
    let mut subjectless = Transformation::for_member_stage(StageId::Construct, digest(9), digest(0xC0));
    subjectless.inputs.clear();
    let payload = DispatchPayload {
        bloom: bloom.0,
        workpiece: WorkpieceId("wp-none".to_owned()),
        stage: StageId::Construct,
        transformation: subjectless,
        scope_revision: digest(9),
        candidate: None,
    };
    store.enqueue_outbox(TOPIC_DISPATCH, &to_vec(&payload).unwrap()).unwrap();
    enqueue_construct_dispatch(&mut store, bloom, "wp-c", 7);

    let (handles, ack_through, _transient_failure) = drain_and_dispatch(&mut store, &shell).unwrap();

    // Only the first entry submitted; the drain broke at the subject-less entry, so
    // the ack prefix stops there rather than jumping past it to the third entry.
    assert_eq!(handles.len(), 1, "only the entry before the subject-less one submitted");
    assert_eq!(ack_through, Some(first), "the ack prefix stops at the last success, not a later one");

    // The subject-less entry and the one behind it re-drain — nothing acked them away.
    store.ack_outbox(Some(TOPIC_DISPATCH), ack_through.unwrap()).unwrap();
    let remaining = store.drain_outbox(Some(TOPIC_DISPATCH)).unwrap();
    assert_eq!(remaining.len(), 2, "the subject-less entry and the one behind it are not acked past");
}

#[test]
fn pull_and_admit_admits_a_matching_construct_result_as_attempt_completed() {
    // The full loop: dispatch a Construct attempt, then a completed run uploads a
    // name-encoded passing result; the pull side decodes it via NameEvidenceClaims,
    // the broker binds it, and the admitted event is a Fact::AttemptCompleted
    // (Construct is non-terminal) advancing the member.
    let mut store = SqliteStore::open(":memory:").unwrap();
    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let bloom = BloomId(digest(1));
    let (sequence, subject) = enqueue_construct_dispatch(&mut store, bloom, "wp-line", 5);

    let (handles, ack_through, _transient_failure) = drain_and_dispatch(&mut store, &shell).unwrap();
    store.ack_outbox(Some(TOPIC_DISPATCH), ack_through.unwrap()).unwrap();
    let mut tracked = track(handles);
    let nonce = format!("dispatch-{sequence}");

    // The run completes and uploads a passing attempt result named so the port's
    // nonce-scoped stream returns it and NameEvidenceClaims decodes it. The
    // subject must equal the displayed digest (the order's) for the broker.
    let run_id = fake.seed_run(&nonce, RunStatus::Completed, Some(RunConclusion::Success));
    let name = attempt_artifact_name(&Nonce(nonce), &digest(subject), StageVerdict::VerificationPassed, &digest(9));
    fake.seed_run_artifacts(run_id, vec![Artifact { id: 1, name, size_bytes: 20 }]);

    let admits = pull_and_admit(&mut store, &shell, NameEvidenceClaims, &mut tracked, None, None, &NopPush);

    assert_eq!(admits.len(), 1, "the matching result admits");
    assert!(tracked.is_empty(), "the admitted order was consumed, so its handle is pruned");
    let event: aether_bloomery::Event = from_bytes(&admits[0].event).unwrap();
    match event.fact {
        Fact::AttemptCompleted { bloom: admitted_bloom, workpiece, stage, passed, .. } => {
            assert_eq!(admitted_bloom, bloom);
            assert_eq!(workpiece, WorkpieceId("wp-line".to_owned()));
            assert_eq!(stage, StageId::Construct);
            assert!(passed, "the VerificationPassed verdict admits as a passing attempt");
        }
        other => panic!("expected a Fact::AttemptCompleted, got {other:?}"),
    }
    // A non-terminal member stage — Construct has a successor in the line.
    assert!(StageCatalog::next_member_stage(StageId::Construct).is_some());
}

#[test]
fn seed_tracked_recovers_a_dispatched_order_across_a_restart() {
    // The restart-shaped bug (#3641): a work order that was submitted and
    // recorded but not yet admitted when the process stopped must be
    // re-tracked at the next boot from the persisted `outstanding_orders`
    // table — `init` used to start `tracked: Vec::new()` unconditionally, so
    // this order would never be polled again.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bloomery.db").to_str().unwrap().to_owned();
    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let bloom = BloomId(digest(1));

    let nonce = {
        let mut store = SqliteStore::open(&path).unwrap();
        let (sequence, _subject) = enqueue_construct_dispatch(&mut store, bloom, "wp-line", 5);
        let (handles, ack_through, _transient_failure) = drain_and_dispatch(&mut store, &shell).unwrap();
        assert_eq!(handles.len(), 1, "the order dispatched and was recorded before the simulated crash");
        store.ack_outbox(Some(TOPIC_DISPATCH), ack_through.unwrap()).unwrap();
        format!("dispatch-{sequence}")
        // `store` drops here — the process stops before this dispatch's
        // result was ever pulled, with no in-memory `tracked` surviving it.
    };

    // Restart: reopen the same file. A fresh driver state's `init` seeds
    // `tracked` from the persisted registry instead of starting empty.
    let mut store = SqliteStore::open(&path).unwrap();
    let mut tracked: Vec<TrackedHandle> = seed_tracked(&mut store)
        .unwrap()
        .into_iter()
        .map(|handle| TrackedHandle::new(handle, Instant::now()))
        .collect();
    assert_eq!(
        tracked.iter().map(|tracked_handle| tracked_handle.handle.nonce.0.clone()).collect::<Vec<_>>(),
        vec![nonce.clone()],
        "the dispatched-but-unresolved order is re-tracked after restart",
    );

    // The run completed while the process was down; the first intake cycle
    // after restart resumes inspecting the seeded handle and admits it.
    let run_id = fake.seed_run(&nonce, RunStatus::Completed, Some(RunConclusion::Success));
    let name = attempt_artifact_name(&Nonce(nonce), &digest(5), StageVerdict::VerificationPassed, &digest(9));
    fake.seed_run_artifacts(run_id, vec![Artifact { id: 1, name, size_bytes: 20 }]);

    let admits = pull_and_admit(&mut store, &shell, NameEvidenceClaims, &mut tracked, None, None, &NopPush);
    assert_eq!(admits.len(), 1, "the restart-seeded handle is inspected and its result admitted, not stranded");
    assert!(tracked.is_empty(), "the order is consumed on admit and no longer tracked");
}

#[test]
fn a_construct_dispatch_runs_local_through_the_routing_shell_and_admits() {
    // The whole local lane end-to-end: a Construct dispatch routes to the local
    // backend (a stub `cargo xtask transform`), the run completes, and the pull
    // side decodes the backend-synthesized evidence name and admits it — no
    // GitHub, no fork, no secret. The construct record shows a substantive
    // conclusion (terminal `result`, is_error == false, a produced candidate), so
    // the gate folds it to a passing attempt (#3596).
    let base = tempfile::TempDir::new().unwrap();
    // A correspondence seeded with the dispatch checkout (`digest(0xC0)`) so both
    // backends resolve it — the local lane checks it out for the `git worktree add`.
    let correspondence: aether_bloomery_github::SharedCorrespondence = {
        let fake = FakeGithub::new();
        fake.seed_git_object(&digest(0xC0));
        Arc::new(fake)
    };
    let actions = Arc::new(ActionsExecutor::new(FakeGithub::new(), Arc::clone(&correspondence), WORKFLOW, PINNED_REF));
    let runner = FixedRunner {
        evidence: r#"{"command":"construct.implement","nonce":"x","produced_candidate":true,"result_record":{"schema":1,"is_error":false,"result":{"num_turns":3}}}"#.to_owned(),
        lifecycle: RunLifecycle::Exited { success: true },
        captures: true,
    };
    let local = Arc::new(LocalExecutor::new(Arc::new(runner), correspondence, base.path(), None, None));
    let routing = RoutingExecutor::new(actions, local, vec!["construct.".to_owned()]);
    let shell = ExecutorShell::new(Arc::new(routing));

    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let (sequence, _subject) = enqueue_construct_dispatch(&mut store, bloom, "wp-local", 5);

    let (handles, ack_through, _transient_failure) = drain_and_dispatch(&mut store, &shell).unwrap();
    store.ack_outbox(Some(TOPIC_DISPATCH), ack_through.unwrap()).unwrap();
    assert_eq!(handles.len(), 1, "the construct order dispatched to the local backend");
    assert_eq!(handles[0].nonce.0, format!("dispatch-{sequence}"), "the handle carries the dispatch nonce");
    let mut tracked = track(handles);

    let admits = pull_and_admit(&mut store, &shell, NameEvidenceClaims, &mut tracked, None, None, &NopPush);
    assert_eq!(admits.len(), 1, "the completed local run's result admits to the control core");
    assert!(tracked.is_empty(), "the admitted order was consumed");
    let event: aether_bloomery::Event = from_bytes(&admits[0].event).unwrap();
    match event.fact {
        Fact::AttemptCompleted { workpiece, stage, passed, .. } => {
            assert_eq!(workpiece, WorkpieceId("wp-local".to_owned()));
            assert_eq!(stage, StageId::Construct);
            assert!(passed, "a local construct success admits as a passing attempt");
        }
        other => panic!("expected a Fact::AttemptCompleted, got {other:?}"),
    }
}

#[test]
fn drain_and_dispatch_parks_a_permanent_refusal_instead_of_re_driving() {
    let mut store = SqliteStore::open(":memory:").unwrap();
    let shell = failing_shell(404);
    let bloom = BloomId(digest(1));
    let (sequence, _subject) = enqueue_construct_dispatch(&mut store, bloom, "wp-wrong-workflow", 5);

    let (handles, ack_through, transient_failure) = drain_and_dispatch(&mut store, &shell).unwrap();

    // The permanent refusal is acked past (parked), not left to re-drive, and it
    // carries no transient-failure sequence for the backoff cursor to key off.
    assert!(handles.is_empty(), "the refused entry never dispatched");
    assert_eq!(ack_through, Some(sequence), "a permanent refusal acks the entry past rather than re-driving it");
    assert_eq!(transient_failure, None, "a permanent park is not a transient failure the backoff cursor tracks");

    store.ack_outbox(Some(TOPIC_DISPATCH), ack_through.unwrap()).unwrap();
    assert!(store.drain_outbox(Some(TOPIC_DISPATCH)).unwrap().is_empty(), "the parked entry does not re-drain");
}

#[test]
fn drain_and_dispatch_leaves_a_transient_refusal_undrained_to_retry() {
    let mut store = SqliteStore::open(":memory:").unwrap();
    let shell = failing_shell(500);
    let bloom = BloomId(digest(1));
    let (sequence, _subject) = enqueue_construct_dispatch(&mut store, bloom, "wp-flaky", 5);

    let (handles, ack_through, transient_failure) = drain_and_dispatch(&mut store, &shell).unwrap();

    assert!(handles.is_empty(), "the refused entry never dispatched");
    assert_eq!(ack_through, None, "a transient refusal stops the ack prefix so the entry re-drains");
    assert_eq!(transient_failure, Some(sequence), "the transient failure names the head sequence for backoff");
    assert_eq!(
        store.drain_outbox(Some(TOPIC_DISPATCH)).unwrap().len(),
        1,
        "the un-acked entry re-drains on the next tick"
    );
}

#[test]
fn backoff_delay_is_monotonic_non_decreasing_and_clamped_at_the_cap() {
    let mut previous = Duration::ZERO;
    for failures in 1..=20 {
        let delay = backoff_delay(failures);
        assert!(delay >= previous, "backoff_delay({failures}) regressed below the prior failure count's delay");
        assert!(delay <= BACKOFF_CAP, "backoff_delay({failures}) exceeded the cap");
        previous = delay;
    }
    assert_eq!(backoff_delay(20), BACKOFF_CAP, "a long failure streak clamps at the cap rather than overflowing");
}

#[test]
fn next_backoff_grows_on_the_same_sequence_and_clears_on_success() {
    let before = Instant::now();

    let first = next_backoff(None, Some(7)).expect("a transient failure opens a cursor");
    assert_eq!(first.sequence, 7);
    assert_eq!(first.failures, 1);
    assert!(first.retry_after >= before + backoff_delay(1));

    let second =
        next_backoff(Some(&first), Some(7)).expect("a repeated failure of the same sequence keeps backing off");
    assert_eq!(second.sequence, 7);
    assert_eq!(second.failures, 2, "consecutive failures of the same head entry grow the count");
    assert!(second.retry_after > first.retry_after, "a grown failure count pushes the retry window further out");

    assert!(next_backoff(Some(&second), None).is_none(), "a success (no transient failure) clears the cursor");
}

#[test]
fn next_backoff_resets_the_count_on_a_changed_head_sequence() {
    let stuck = next_backoff(None, Some(7)).unwrap();
    let stuck = next_backoff(Some(&stuck), Some(7)).unwrap();
    assert_eq!(stuck.failures, 2);

    // Sequence 7 cleared some other way (parked, or a later success) and a new
    // head entry, sequence 9, now fails transiently — the count restarts at 1
    // rather than continuing to grow the prior entry's tally.
    let restarted = next_backoff(Some(&stuck), Some(9)).unwrap();
    assert_eq!(restarted.sequence, 9);
    assert_eq!(restarted.failures, 1, "a changed head sequence resets the consecutive-failure count");
}

#[test]
fn is_stale_is_true_only_at_or_past_the_threshold() {
    let first_seen = Instant::now();
    let threshold = Duration::from_mins(30);
    assert!(!is_stale(first_seen, (first_seen + threshold).checked_sub(Duration::from_secs(1)).unwrap(), threshold));
    assert!(is_stale(first_seen, first_seen + threshold, threshold));
    assert!(is_stale(first_seen, first_seen + Duration::from_hours(1), threshold));
}

#[test]
fn select_stale_handles_warns_once_for_a_wedged_handle_and_never_for_a_fresh_one() {
    // #3635: a handle tracked past the threshold selects for exactly one warning
    // naming its last observed (pending) status; a handle tracked well within the
    // threshold never selects, and a repeat sweep at the same instant does not
    // re-select the already-warned handle.
    let now = Instant::now();
    let threshold = Duration::from_mins(30);
    let wedged_nonce = Nonce("dispatch-wedged".to_owned());
    let fresh_nonce = Nonce("dispatch-fresh".to_owned());
    let mut tracked = vec![
        TrackedHandle::new(
            WorkHandle::new(wedged_nonce.clone()),
            now.checked_sub(threshold + Duration::from_secs(1)).unwrap(),
        ),
        TrackedHandle::new(WorkHandle::new(fresh_nonce.clone()), now.checked_sub(Duration::from_secs(5)).unwrap()),
    ];
    let pending = vec![(wedged_nonce.clone(), ExecutionStatus::Running), (fresh_nonce, ExecutionStatus::Queued)];

    let warnings = select_stale_handles(&mut tracked, &pending, now, Some(threshold));
    assert_eq!(warnings.len(), 1, "only the past-threshold handle selects");
    let (nonce, age, status) = &warnings[0];
    assert_eq!(*nonce, wedged_nonce);
    assert!(*age >= threshold);
    assert_eq!(*status, ExecutionStatus::Running, "the wedged handle's warning carries its last observed status");

    let repeat = select_stale_handles(&mut tracked, &pending, now, Some(threshold));
    assert!(repeat.is_empty(), "an already-warned handle does not re-select on the next sweep");
}

#[test]
fn select_stale_handles_selects_nothing_when_the_sweep_is_disabled() {
    let now = Instant::now();
    let wedged_nonce = Nonce("dispatch-wedged".to_owned());
    let first_seen = now.checked_sub(Duration::from_hours(1)).unwrap();
    let mut tracked = vec![TrackedHandle::new(WorkHandle::new(wedged_nonce), first_seen)];
    assert!(select_stale_handles(&mut tracked, &[], now, None).is_empty(), "threshold: None disables the sweep");
}

// ADR-0152 — the record's axes come from the payload's explicit fields: with a
// candidate present, the displayed digest (what returning evidence must bind) is
// the candidate tree while the scope revision stays the true revision. Catches
// the placeholder regression (all three stamped from inputs[0]).
#[test]
fn drain_stamps_the_record_axes_from_the_payload() {
    let mut store = SqliteStore::open(":memory:").unwrap();
    let backend = Arc::new(CapturingBackend::default());
    let shell = ExecutorShell::new(Arc::clone(&backend));
    let bloom = BloomId(digest(1));
    let candidate_tree = digest(0xAB);
    let payload = DispatchPayload {
        bloom: bloom.0,
        workpiece: WorkpieceId("wp-cand".to_owned()),
        stage: StageId::Verify,
        transformation: Transformation::for_member_stage(StageId::Verify, candidate_tree, digest(0xC0)),
        scope_revision: digest(5),
        candidate: Some(candidate_tree),
    };
    let sequence = store.enqueue_outbox(TOPIC_DISPATCH, &to_vec(&payload).unwrap()).unwrap();

    drain_and_dispatch(&mut store, &shell).unwrap();

    let order = store.lookup_order(&format!("dispatch-{sequence}")).unwrap().expect("the order recorded");
    assert_eq!(order.scope_revision, digest(5).as_bytes().to_vec(), "the true scope revision survives");
    assert_eq!(order.displayed_digest, candidate_tree.as_bytes().to_vec(), "evidence binds the candidate tree");
    assert_eq!(order.candidate, candidate_tree.as_bytes().to_vec());
}

// ADR-0152 — an admitted passing capture is pushed to the bloom's candidate ref,
// resolved through the correspondence to the capture commit; a failing or
// capture-less admission pushes nothing. Catches a push against the wrong ref
// namespace (a downstream Actions checkout would 404) and a push of discarded
// (failing) work.
#[test]
fn admitted_passing_captures_push_to_the_bloom_candidate_ref() {
    use aether_bloomery::{CandidateRef, Event, IdempotencyKey};
    use aether_bloomery_github::Correspondence as _;

    let bloom = BloomId(digest(1));
    let capture = CandidateRef { tree: digest(0xAB), checkout: digest(0xAC) };
    let store = FakeGithub::new();
    store.seed_git_object(&capture.checkout);
    let commit_hex = store.resolve_git(&capture.checkout).unwrap().unwrap().to_hex();
    let admission = |passed: bool, candidate: Option<CandidateRef>| {
        let fact = Fact::AttemptCompleted {
            bloom,
            workpiece: WorkpieceId("wp/cand".to_owned()),
            stage: StageId::Construct,
            passed,
            evidence: aether_bloomery::Evidence {
                subject: digest(9),
                kind: aether_bloomery::EvidenceKind::VerificationResult,
                detail: digest(8),
            },
            candidate,
        };
        let event = Event { idempotency_key: IdempotencyKey("k".to_owned()), fact };
        Admission { admit: aether_bloomery::Admit { event: to_vec(&event).unwrap() }, event }
    };

    let pusher = RecordingPush::default();
    let correspondence: aether_bloomery_github::SharedCorrespondence = Arc::new(store);
    push_admitted_candidates(
        &[admission(true, Some(capture)), admission(false, Some(capture)), admission(true, None)],
        Some(&correspondence),
        &pusher,
    );

    let issued = pusher.pushed.lock().unwrap().clone();
    assert_eq!(issued.len(), 1, "only the passing capture pushes");
    assert_eq!(issued[0].0, commit_hex, "the pushed sha is the capture commit, resolved via correspondence");
    assert_eq!(
        issued[0].1,
        candidate_ref_name(&bloom, "wp/cand"),
        "the target is the bloom candidate ref for the workpiece",
    );
    assert!(issued[0].1.starts_with("refs/heads/bloom/"), "the ref lives in the bloom namespace");
    assert!(issued[0].1.ends_with("/candidate/wp-cand"), "the workpiece segment is sanitized to ref-safe characters");
}

// #3656 / ADR-0153 — persisted findings from the failing verdict compose onto
// the construct-lane dispatch as their own labeled section after the work-order
// description, so a Refine re-entry's prompt names both the original order and
// what the failing gate flagged. Catches the findings never reaching the model
// (a blind re-entry).
#[test]
fn drain_threads_persisted_findings_onto_the_construct_order() {
    let mut store = SqliteStore::open(":memory:").unwrap();
    let backend = Arc::new(CapturingBackend::default());
    let shell = ExecutorShell::new(Arc::clone(&backend));
    let bloom = BloomId(digest(1));
    store.record_dispatch_description(bloom.0.as_bytes(), "wp-line", "the original order").unwrap();
    store.record_review_findings(bloom.0.as_bytes(), "wp-line", "clippy: off-by-one in the loop bound").unwrap();
    enqueue_construct_dispatch(&mut store, bloom, "wp-line", 5);

    drain_and_dispatch(&mut store, &shell).unwrap();

    let orders = backend.orders();
    let description = orders[0].transformation.description.as_deref().unwrap();
    assert!(description.starts_with("the original order"), "the order text leads");
    assert!(
        description.contains("## Findings\n\nclippy: off-by-one in the loop bound"),
        "the findings follow as their own labeled section",
    );
}
