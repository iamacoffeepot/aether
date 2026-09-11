//! Durable shared-run identity, recovery, and regrouping contracts.

use std::slice::from_ref;

use super::{
    QueuedMemberVerificationRow, RecordOutcome, SharedRunLifecycle, SharedRunMemberRow, SharedRunRow, SharedRunStepRow,
    SqliteStore, StoreBackend,
};

fn run(seed: u8) -> SharedRunRow {
    SharedRunRow {
        run: vec![seed; 32],
        nonce: format!("physical-{seed}"),
        dispatch: vec![seed; 8],
        lifecycle: SharedRunLifecycle::Preparing,
        next_ordinal: 0,
        deadline_unix_millis: 11_000,
        charged: false,
        physical_cost: None,
    }
}

fn member(run: &SharedRunRow, request: u8, ordinal: u32) -> SharedRunMemberRow {
    SharedRunMemberRow {
        run: run.run.clone(),
        request: vec![request; 32],
        ordinal,
        queued_unix_millis: 1_000,
        deadline_unix_millis: 11_000,
        cancelled: false,
        outcome: None,
        latency_millis: None,
    }
}

fn step(run: &SharedRunRow) -> SharedRunStepRow {
    SharedRunStepRow {
        run: run.run.clone(),
        ordinal: 0,
        nonce: format!("{}-step-0", run.nonce),
        request: None,
        descriptor: vec![19],
        prepared: None,
        receipt: None,
        duration_millis: None,
        release_physical_run: false,
    }
}

#[test]
fn reopening_replays_the_original_deadline_and_partial_receipt() {
    let directory = tempfile::tempdir().expect("temporary store directory");
    let path = directory.path().join("shared.sqlite");
    let path = path.to_str().expect("temporary UTF-8 path");
    let original = run(1);
    let participant = member(&original, 21, 0);
    let invocation = step(&original);
    {
        let mut store = SqliteStore::open(path).expect("open store");
        store.record_shared_run(&original, from_ref(&participant)).expect("record run");
        store.record_shared_run_step(&invocation).expect("record invocation intent");
        assert!(store.prepare_shared_run_step(&invocation.nonce, &[31]).expect("retain preparation"));
        assert!(store.complete_shared_run_step(&invocation.nonce, &[41], 5_200).expect("retain result"));
        store.update_shared_run(&original.run, SharedRunLifecycle::Running, 1).expect("advance durable cursor");
    }
    let mut store = SqliteStore::open(path).expect("reopen store");
    let mut replayed = original.clone();
    replayed.deadline_unix_millis = 99_000;
    let mut replayed_participant = participant.clone();
    replayed_participant.queued_unix_millis = 89_000;
    replayed_participant.deadline_unix_millis = 99_000;
    assert_eq!(
        store.record_shared_run(&replayed, &[replayed_participant]).expect("replay same intent"),
        RecordOutcome::Duplicate
    );
    let retained = store.lookup_shared_run(&original.run).expect("read run").expect("retained run");
    assert_eq!(retained.deadline_unix_millis, original.deadline_unix_millis);
    assert_eq!(retained.lifecycle, SharedRunLifecycle::Running);
    assert_eq!(retained.next_ordinal, 1);
    assert_eq!(store.shared_run_members(&original.run).expect("read membership"), [participant]);
    let retained = store.shared_run_steps(&original.run).expect("read invocations");
    assert_eq!(retained[0].prepared, Some(vec![31]));
    assert_eq!(retained[0].receipt, Some(vec![41]));
    assert_eq!(retained[0].duration_millis, Some(5_200));
}

#[test]
fn immutable_run_and_step_ids_refuse_changed_payloads() {
    let mut store = SqliteStore::open(":memory:").expect("store");
    let original = run(1);
    let participant = member(&original, 21, 0);
    store.record_shared_run(&original, from_ref(&participant)).expect("record run");
    let mut changed = original.clone();
    changed.dispatch.push(99);
    assert!(store.record_shared_run(&changed, from_ref(&participant)).is_err());
    let mut alias = run(2);
    alias.nonce = original.nonce.clone();
    assert!(store.record_shared_run(&alias, &[member(&alias, 22, 0)]).is_err());
    assert!(store.lookup_shared_run(&alias.run).expect("read refused alias").is_none());

    let original_step = step(&original);
    store.record_shared_run_step(&original_step).expect("record invocation");
    let mut changed_step = original_step.clone();
    changed_step.descriptor.push(99);
    assert!(store.record_shared_run_step(&changed_step).is_err());
    assert_eq!(store.shared_run_steps(&original.run).expect("read invocations"), [original_step]);
    assert_eq!(store.lookup_shared_run(&original.run).expect("read run"), Some(original));
}

#[test]
fn regrouping_preserves_the_queue_time_and_gives_the_new_attempt_its_own_deadline() {
    let mut store = SqliteStore::open(":memory:").expect("store");
    let first = run(1);
    let second = run(2);
    let original = member(&first, 21, 0);
    store.record_shared_run(&first, from_ref(&original)).expect("first association");
    store.update_shared_run(&first.run, SharedRunLifecycle::Completed, 0).expect("first physical run ended");
    let mut regrouped = member(&second, 21, 0);
    regrouped.queued_unix_millis = 9_000;
    regrouped.deadline_unix_millis = 19_000;
    store.record_shared_run(&second, &[regrouped.clone()]).expect("same logical request can regroup");
    let retained = store.shared_run_members(&second.run).expect("second association");
    assert_eq!(
        retained[0].queued_unix_millis, original.queued_unix_millis,
        "latency still spans from the logical request's first enqueue"
    );
    assert_eq!(
        retained[0].deadline_unix_millis, regrouped.deadline_unix_millis,
        "the regrouped attempt runs against its own wall clock, not the spent one"
    );
    assert!(
        store
            .record_shared_run_member_outcome(&second.run, &original.request, &[51], 9_500)
            .expect("second run outcome")
    );
    assert_eq!(store.shared_run_members(&first.run).expect("historical membership"), [original]);
    let second_members = store.shared_run_members(&second.run).expect("current membership");
    assert_eq!(second_members[0].outcome, Some(vec![51]));
    assert_eq!(second_members[0].latency_millis, Some(9_500));
}

#[test]
fn logical_cancellation_preserves_completed_results_and_other_members() {
    let mut store = SqliteStore::open(":memory:").expect("store");
    let first = run(1);
    let second = run(2);
    let request = member(&first, 21, 0);
    let sibling = member(&first, 22, 1);
    store.record_shared_run(&first, &[request.clone(), sibling]).expect("first run");
    store.record_shared_run(&second, &[member(&second, 21, 0), member(&second, 22, 1)]).expect("second run");
    store.record_shared_run_member_outcome(&first.run, &request.request, &[51], 5_000).expect("retained completion");
    assert!(store.cancel_shared_run_member(&request.request).expect("cancel unfinished associations"));
    let first_members = store.shared_run_members(&first.run).expect("first members");
    assert_eq!(first_members[0].outcome, Some(vec![51]));
    assert!(!first_members[0].cancelled);
    assert!(!first_members[1].cancelled);
    let second_members = store.shared_run_members(&second.run).expect("second members");
    assert!(second_members[0].cancelled);
    assert!(!second_members[1].cancelled);
}

#[test]
fn a_re_queued_request_runs_against_a_fresh_deadline() {
    // A retry re-queues the identical request digest, so every attempt after
    // the first would inherit the first attempt's wall clock: selection skips
    // it, submission mints an expired executor fault, and a passed member is
    // rewritten back to pending — all before the retry has run at all.
    let mut store = SqliteStore::open(":memory:").expect("store");
    let first = QueuedMemberVerificationRow {
        request: vec![7; 32],
        sequence: 1,
        payload: vec![11, 12, 13],
        queued_unix_millis: 1_000,
        deadline_unix_millis: 61_000,
        scheduled: false,
        proposal: None,
    };
    assert_eq!(store.record_queued_member_verification(&first).expect("first enqueue"), RecordOutcome::Recorded);
    store.mark_queued_member_verifications_scheduled(from_ref(&first.request)).expect("first attempt selected");

    let retry = QueuedMemberVerificationRow {
        sequence: 4,
        queued_unix_millis: 55_000,
        deadline_unix_millis: 115_000,
        ..first.clone()
    };
    assert_eq!(store.record_queued_member_verification(&retry).expect("re-queue"), RecordOutcome::Recorded);

    let retained = store.queued_member_verification(&first.request).expect("lookup").expect("row");
    assert_eq!(retained.deadline_unix_millis, retry.deadline_unix_millis);
    assert_eq!(retained.queued_unix_millis, first.queued_unix_millis, "latency is still measured from first arrival");
}

#[test]
fn duplicate_members_roll_back_the_entire_physical_run() {
    let mut store = SqliteStore::open(":memory:").expect("store");
    let physical = run(1);
    let participant = member(&physical, 21, 0);
    assert!(store.record_shared_run(&physical, &[participant.clone(), participant]).is_err());
    assert!(store.lookup_shared_run(&physical.run).expect("read rolled-back run").is_none());
    assert!(store.shared_run_members(&physical.run).expect("read rolled-back members").is_empty());
}
