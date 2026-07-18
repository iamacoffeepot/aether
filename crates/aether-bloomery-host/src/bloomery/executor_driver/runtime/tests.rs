//! The drain → submit → record and pull → admit core of the executor dispatch
//! driver, over a real `SqliteStore` and a fake-GitHub-backed `ExecutorShell` —
//! the network side the running capability drives, without the mail harness.
//! `init` / the timer / the ctx send are the thin glue the chassis-boot test and
//! compilation cover; this pins the loop that actually dispatches and admits.

use std::sync::Arc;

use aether_bloomery::{BloomId, DispatchPayload, Fact, StageCatalog, StageId, Transformation, WorkpieceId};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{ActionsExecutor, Artifact, RunConclusion, RunStatus, StageVerdict};
use aether_data::wire::{from_bytes, to_vec};

use super::{DISPATCH_TOPIC, NameEvidenceClaims, drain_and_dispatch, pull_and_admit};
use crate::bloomery::intake::attempt_artifact_name;
use crate::bloomery::local_executor::testing::FixedRunner;
use crate::bloomery::{ExecutorShell, LocalExecutor, RoutingExecutor, RunLifecycle};
use crate::store::{SqliteStore, StoreBackend};

const WORKFLOW: &str = "bloomery-transform.yml";
const PINNED_REF: &str = "refs/heads/main";

fn shell(fake: FakeGithub) -> ExecutorShell {
    // The dispatched orders check out `digest(0xC0)`; seed its correspondence so
    // `submit` resolves the subject (the fake's store is shared across clones).
    fake.seed_git_object(&digest(0xC0));
    ExecutorShell::new(Arc::new(ActionsExecutor::new(fake.clone(), Arc::new(fake), WORKFLOW, PINNED_REF)))
}

fn digest(seed: u8) -> aether_bloomery::Digest {
    aether_bloomery::Digest::from_bytes([seed; 32])
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
    };
    let sequence = store.enqueue_outbox(DISPATCH_TOPIC, &to_vec(&payload).unwrap()).unwrap();
    (sequence, subject)
}

#[test]
fn drain_and_dispatch_submits_each_dispatch_and_records_its_order() {
    let mut store = SqliteStore::open(":memory:").unwrap();
    let fake = FakeGithub::new();
    let shell = shell(fake.clone());
    let bloom = BloomId(digest(1));
    let (sequence, _subject) = enqueue_construct_dispatch(&mut store, bloom, "wp-line", 5);

    let (handles, ack_through) = drain_and_dispatch(&mut store, &shell).unwrap();

    // One dispatch submitted, its order recorded, and the ack prefix covers it.
    assert_eq!(handles.len(), 1);
    assert_eq!(ack_through, Some(sequence));
    let nonce = format!("dispatch-{sequence}");
    assert_eq!(fake.dispatched_nonces(), vec![nonce.clone()], "the work order reached the executor");
    let order = store.lookup_order(&nonce).unwrap().expect("the intake registry recorded the order");
    assert_eq!(order.workpiece, "wp-line");

    // Acking the prefix means the entry does not re-drain.
    store.ack_outbox(Some(DISPATCH_TOPIC), sequence).unwrap();
    assert!(store.drain_outbox(Some(DISPATCH_TOPIC)).unwrap().is_empty(), "the acked dispatch does not re-drain");
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
    };
    store.enqueue_outbox(DISPATCH_TOPIC, &to_vec(&payload).unwrap()).unwrap();
    enqueue_construct_dispatch(&mut store, bloom, "wp-c", 7);

    let (handles, ack_through) = drain_and_dispatch(&mut store, &shell).unwrap();

    // Only the first entry submitted; the drain broke at the subject-less entry, so
    // the ack prefix stops there rather than jumping past it to the third entry.
    assert_eq!(handles.len(), 1, "only the entry before the subject-less one submitted");
    assert_eq!(ack_through, Some(first), "the ack prefix stops at the last success, not a later one");

    // The subject-less entry and the one behind it re-drain — nothing acked them away.
    store.ack_outbox(Some(DISPATCH_TOPIC), ack_through.unwrap()).unwrap();
    let remaining = store.drain_outbox(Some(DISPATCH_TOPIC)).unwrap();
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

    let (mut tracked, ack_through) = drain_and_dispatch(&mut store, &shell).unwrap();
    store.ack_outbox(Some(DISPATCH_TOPIC), ack_through.unwrap()).unwrap();
    let nonce = format!("dispatch-{sequence}");

    // The run completes and uploads a passing attempt result named so the port's
    // nonce-scoped stream returns it and NameEvidenceClaims decodes it. The
    // subject must equal the displayed digest (the order's) for the broker.
    let run_id = fake.seed_run(&nonce, RunStatus::Completed, Some(RunConclusion::Success));
    let name = attempt_artifact_name(
        &aether_bloomery::Nonce(nonce),
        &digest(subject),
        StageVerdict::VerificationPassed,
        &digest(9),
    );
    fake.seed_run_artifacts(run_id, vec![Artifact { id: 1, name, size_bytes: 20 }]);

    let admits = pull_and_admit(&mut store, &shell, NameEvidenceClaims, &mut tracked);

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
fn a_construct_dispatch_runs_local_through_the_routing_shell_and_admits() {
    // The whole local lane end-to-end: a Construct dispatch routes to the local
    // backend (a stub `cargo xtask transform`), the run completes, and the pull
    // side decodes the backend-synthesized evidence name and admits it — no
    // GitHub, no fork, no secret. The construct record carries no `status`, so the
    // verdict folds from the (stubbed) success exit to a passing attempt.
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
        evidence: r#"{"command":"construct.implement","nonce":"x","result_record":{"schema":1}}"#.to_owned(),
        lifecycle: RunLifecycle::Exited { success: true },
    };
    let local = Arc::new(LocalExecutor::new(Arc::new(runner), correspondence, base.path(), None, None));
    let routing = RoutingExecutor::new(actions, local, vec!["construct.".to_owned()]);
    let shell = ExecutorShell::new(Arc::new(routing));

    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = BloomId(digest(1));
    let (sequence, _subject) = enqueue_construct_dispatch(&mut store, bloom, "wp-local", 5);

    let (mut tracked, ack_through) = drain_and_dispatch(&mut store, &shell).unwrap();
    store.ack_outbox(Some(DISPATCH_TOPIC), ack_through.unwrap()).unwrap();
    assert_eq!(tracked.len(), 1, "the construct order dispatched to the local backend");
    assert_eq!(tracked[0].nonce.0, format!("dispatch-{sequence}"), "the handle carries the dispatch nonce");

    let admits = pull_and_admit(&mut store, &shell, NameEvidenceClaims, &mut tracked);
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
