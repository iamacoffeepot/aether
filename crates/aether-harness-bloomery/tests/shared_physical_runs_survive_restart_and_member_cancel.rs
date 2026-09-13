//! Shared verification crosses the real coordinator, local executor, intake,
//! and durable-store boundaries. These scenarios keep that path honest across
//! the two interruptions that used to lose physical-run progress.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    BloomStatus, CommissionStatus, CoordinationPolicy, Digest, FakeKeyProvider, KeyId, MemberVerifyOutcome,
    ResolutionProof, ResolvedConfigs, SharedRunDispatch, SharedRunMode, SharedRunPhase, Snapshot, StageId,
    VerificationMode, WorkpieceId, decode_recorded_decisions, decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, SharedRunLifecycle, SharedRunRow, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, OperatorMove, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";

fn warm_serial_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        verification: VerificationMode::WarmSerial,
        eager_integration: false,
        max_run_members: 2,
        max_serial_requests: 2,
        max_attribution_probes: 0,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
    }
}

fn contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        verification: VerificationMode::Contextual,
        eager_integration: false,
        max_run_members: 2,
        max_serial_requests: 2,
        max_attribution_probes: 2,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("shared-run harness")), &[0x0A; 32], *scope_revision);
        store
            .insert_approval(&approval, &FakeKeyProvider)
            .expect("the original member scope retains its signed approval");
    }
}

fn replay_snapshot(store: &mut dyn StoreBackend) -> Snapshot {
    store.replay_journal().expect("the coordinator journal replays").into_iter().fold(
        Snapshot::default(),
        |snapshot, row| {
            let event = decode_recorded_event(&row.event, row.event_schema.as_deref())
                .expect("the harness journal event decodes");
            let decisions = decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
                .expect("the harness journal decisions decode");
            snapshot.apply(&event, &decisions, &ResolvedConfigs::default())
        },
    )
}

fn complete_joint_constructions(harness: &mut ScenarioHarness) {
    harness.hold_member_verification(true);
    harness.pump_until("both members receive their parked Construct orders", |harness| {
        let orders = harness.orders();
        assert!(orders.len() <= 2, "only the two sealed members can be dispatched: {orders:?}");
        orders.len() == 2
    });
    let constructs = harness.orders();
    assert!(
        constructs
            .iter()
            .all(|order| from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::Construct)),
        "the barrier holds exactly the two Construct orders: {constructs:?}"
    );
    assert!(
        [FIRST, SECOND].iter().all(|workpiece| constructs.iter().any(|order| order.workpiece == *workpiece)),
        "the barrier holds both sealed members: {constructs:?}"
    );

    harness.pump_until("both parked Construct children reached their real worktrees", |harness| {
        let runs = harness.ledger();
        constructs.iter().all(|order| runs.iter().any(|run| run.nonce == order.nonce))
    });
    let runs = harness.ledger();
    for (workpiece, path) in
        [(FIRST, "crates/example-a/src/contextual.rs"), (SECOND, "crates/example-b/src/contextual.rs")]
    {
        let order = constructs
            .iter()
            .find(|order| order.workpiece == workpiece)
            .expect("the jointly admitted member has a Construct order");
        let worktree = runs
            .iter()
            .find(|run| run.nonce == order.nonce)
            .and_then(|run| run.worktree.as_deref())
            .map(Path::new)
            .expect("the parked local Construct recorded its real worktree");
        fs::remove_file(worktree.join(CANDIDATE_FILE)).expect("the mock's generic candidate is removed");
        fs::write(worktree.join(path), format!("pub const MEMBER: &str = \"{workpiece}\";\n"))
            .expect("the parked Construct writes its own approved surface");
    }

    for (index, workpiece) in [FIRST, SECOND].iter().enumerate() {
        let order = constructs.iter().find(|order| order.workpiece == *workpiece).expect("member Construct order");
        harness.release_parked_lanes(from_ref(order));
        harness.pump_until("the captured candidate queues while verification service is held", |harness| {
            let mut store = harness.commission_store();
            assert!(store.list_shared_runs().expect("shared runs read while service is held").is_empty());
            store.queued_member_verifications().expect("logical verification requests read").len() == index + 1
        });
    }
    harness.hold_member_verification(false);
}

fn completed_contextual_run(store: &mut dyn StoreBackend) -> (SharedRunRow, SharedRunDispatch) {
    let decoded = store
        .list_shared_runs()
        .expect("shared runs read after contextual completion")
        .into_iter()
        .map(|run| {
            let dispatch = from_bytes::<SharedRunDispatch>(&run.dispatch).expect("the retained dispatch decodes");
            (run, dispatch)
        })
        .collect::<Vec<_>>();
    let summary = decoded
        .iter()
        .map(|(run, dispatch)| {
            format!(
                "{:?}/{:?} members={:?} charged={}",
                run.lifecycle,
                dispatch.plan.mode,
                dispatch.plan.requests.iter().map(|request| &request.member.workpiece.0).collect::<Vec<_>>(),
                run.charged,
            )
        })
        .collect::<Vec<_>>();
    decoded
        .into_iter()
        .find(|(run, dispatch)| {
            run.lifecycle == SharedRunLifecycle::Completed
                && dispatch.plan.mode == SharedRunMode::Contextual
                && dispatch.plan.requests.iter().map(|request| request.member.workpiece.0.as_str()).eq([FIRST, SECOND])
        })
        .unwrap_or_else(|| panic!("A and B need one completed contextual physical run; observed {summary:?}"))
}

#[test]
fn a_partial_shared_run_restarts_from_its_first_missing_receipt() {
    let roots = HarnessRoots::create();
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FIRST, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Verify, LaneMode::NeverExits);
    let (bloom, first_verify_runs) = {
        let mut harness = HarnessBuilder::local_authority(&authority)
            .roots(&roots)
            .coordination(warm_serial_policy())
            .script(&script)
            .start("partial-shared-run-before-restart");
        let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
        let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
        approve_scopes(&harness, &[first, second]);
        let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);
        complete_joint_constructions(&mut harness);

        harness.pump_until("the second serial member starts after the first receipt", |harness| {
            harness
                .ledger()
                .iter()
                .any(|run| run.workpiece.as_deref() == Some(SECOND) && run.stage == Some(StageId::Verify))
        });
        let first_verify_runs = harness
            .ledger()
            .iter()
            .filter(|run| run.workpiece.as_deref() == Some(FIRST) && run.stage == Some(StageId::Verify))
            .count();
        assert_eq!(first_verify_runs, 1, "the first serial member completed once before restart");

        let mut store = harness.commission_store();
        let open = store
            .list_shared_runs()
            .expect("shared runs read")
            .into_iter()
            .find(|run| run.lifecycle != SharedRunLifecycle::Completed)
            .expect("the interrupted physical run remains durable");
        let steps = store.shared_run_steps(&open.run).expect("shared steps read");
        assert!(steps.iter().any(|step| step.receipt.is_some()), "the completed sibling receipt is durable");
        assert!(steps.iter().any(|step| step.receipt.is_none()), "the interrupted sibling remains replayable");
        (bloom, first_verify_runs)
    };

    let mut restarted = HarnessBuilder::local_authority(&authority)
        .roots(&roots)
        .coordination(warm_serial_policy())
        .script(&LaneScript::all_passing())
        .start("partial-shared-run-after-restart");
    restarted.pump_until("both logical members settle after replay", |harness| {
        harness.bloom(bloom).members.iter().all(|member| member.resolution.is_some())
    });
    let replayed_first = restarted
        .ledger()
        .iter()
        .filter(|run| run.workpiece.as_deref() == Some(FIRST) && run.stage == Some(StageId::Verify))
        .count();
    assert_eq!(
        replayed_first, first_verify_runs,
        "restart resumes the missing serial step without repeating a durable receipt"
    );
}

#[test]
fn cancelling_one_live_serial_member_preserves_its_completed_sibling() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FIRST, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Verify, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(warm_serial_policy())
        .script(&script)
        .start("cancel-one-shared-member");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first, second]);
    let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);
    complete_joint_constructions(&mut harness);

    harness.pump_until("the second serial member starts", |harness| {
        harness
            .ledger()
            .iter()
            .any(|run| run.workpiece.as_deref() == Some(SECOND) && run.stage == Some(StageId::Verify))
    });
    {
        let mut store = harness.commission_store();
        let (run, dispatch) = store
            .list_shared_runs()
            .expect("shared runs read before member cancellation")
            .into_iter()
            .filter(|run| run.lifecycle != SharedRunLifecycle::Completed)
            .find_map(|run| {
                let dispatch = from_bytes::<SharedRunDispatch>(&run.dispatch).ok()?;
                let members = dispatch
                    .plan
                    .requests
                    .iter()
                    .map(|request| request.member.workpiece.0.as_str())
                    .collect::<Vec<_>>();
                (dispatch.plan.mode == SharedRunMode::WarmSerial && members == [FIRST, SECOND])
                    .then_some((run, dispatch))
            })
            .expect("A and B share one live warm-serial physical run");
        let first_request = dispatch
            .plan
            .requests
            .iter()
            .find(|request| request.member.workpiece.0 == FIRST)
            .expect("the coalesced run retains A")
            .digest();
        let second_request = dispatch
            .plan
            .requests
            .iter()
            .find(|request| request.member.workpiece.0 == SECOND)
            .expect("the coalesced run retains B")
            .digest();
        let members = store.shared_run_members(&run.run).expect("shared member associations read");
        assert_eq!(members.len(), 2, "the physical run has exactly the two sealed logical requests");
        assert!(
            members.iter().any(|member| member.request == first_request.as_bytes()),
            "A is associated with the same physical run as B"
        );
        assert!(
            members.iter().any(|member| member.request == second_request.as_bytes()),
            "B is associated with the same physical run as A"
        );

        let steps = store.shared_run_steps(&run.run).expect("shared steps read before cancellation");
        let first = steps
            .iter()
            .find(|step| step.request.as_deref() == Some(first_request.as_bytes().as_slice()))
            .expect("A has a physical step in the coalesced run");
        let second = steps
            .iter()
            .find(|step| step.request.as_deref() == Some(second_request.as_bytes().as_slice()))
            .expect("B has a physical step in the coalesced run");
        assert!(first.receipt.is_some(), "A's receipt completed inside the shared physical run");
        assert!(second.receipt.is_none(), "B remains outstanding inside that same physical run");
    }
    let outcome = harness.apply_operator(
        bloom,
        &OperatorMove::Withdraw {
            at_tick: 0,
            workpiece: WorkpieceId(SECOND.to_owned()),
            reason: "cancel only this logical request".to_owned(),
            operator: "harness".to_owned(),
            cascade: false,
        },
    );
    assert!(format!("{outcome:?}").contains("MembersWithdrawn"), "the member withdrawal is admitted: {outcome:?}");
    harness.pump_until("the cancellation settles without rolling back its sibling", |harness| {
        let view = harness.bloom(bloom);
        let first = view.members.iter().find(|member| member.workpiece.0 == FIRST).unwrap();
        let second = view.members.iter().find(|member| member.workpiece.0 == SECOND).unwrap();
        first.resolution.is_some() && second.withdrawn.is_some()
    });

    let view = harness.bloom(bloom);
    let first = view.members.iter().find(|member| member.workpiece.0 == FIRST).unwrap();
    assert!(first.resolution.is_some(), "member cancellation preserves the sibling's completed shared receipt");
}

#[test]
fn contextual_shared_run_resolves_the_full_root_without_standalone_proofs() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then_for(FIRST, StageId::Construct, LaneMode::NeverExits).then_for(
        SECOND,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        .script(&script)
        .start("contextual-shared-run");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first, second]);
    let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);
    complete_joint_constructions(&mut harness);

    harness.pump_until("the contextual bloom resolves its complete integration root", |harness| {
        let snapshot = replay_snapshot(&mut harness.commission_store());
        snapshot.blooms.get(&bloom).is_some_and(|record| record.resolved_head.is_some())
            && harness
                .commission_store()
                .list_shared_runs()
                .expect("physical release acknowledgements remain readable")
                .iter()
                .all(|run| run.lifecycle == SharedRunLifecycle::Completed)
    });

    let mut store = harness.commission_store();
    let (physical, dispatch) = completed_contextual_run(&mut store);
    let steps = store.shared_run_steps(&physical.run).expect("contextual physical steps read");
    assert!(!steps.is_empty(), "the contextual physical run records its execution step");
    assert!(steps.iter().all(|step| step.receipt.is_some()), "every contextual execution step has settled");

    let snapshot = replay_snapshot(&mut store);
    let record = snapshot.blooms.get(&bloom).expect("the contextual bloom remains projected");
    let state = record.coordination.as_ref().expect("the contextual coordination state remains projected");
    assert_eq!(record.resolved_tree, Some(state.integration.head.candidate.tree));
    assert_eq!(record.resolved_head, Some(state.integration.head.candidate.checkout));
    assert_eq!(state.integration.head.coverage.len(), 2);
    assert_eq!(state.claims.len(), 2);
    let run = state.run(dispatch.plan.digest()).expect("the executed contextual plan remains projected");
    assert_eq!(run.phase, SharedRunPhase::Terminal, "the contextual logical run reached its terminal phase");
    assert!(run.physical_run.is_some(), "the logical run retains its real physical-run identity");
    assert!(
        run.completed.iter().all(|outcome| matches!(outcome, MemberVerifyOutcome::PassedIn { .. })),
        "the aggregate receipt settles every member in the contextual position"
    );
    let node = run.node.as_ref().expect("the contextual run retains its immutable prepared node");
    assert_eq!(node.plan, dispatch.plan.digest(), "the node is bound to the exact dispatched shared plan");
    assert_eq!(
        state.integration.head.candidate, node.candidate,
        "the selected full root retains the exact verified tree and checkout"
    );
    assert_eq!(state.integration.head.coverage, node.coverage, "the selected full root retains exact member coverage");
    assert!(
        state.contextual_aggregate_proof(&state.integration.head).is_some(),
        "the exact selected root reuses its contextual verification receipt"
    );
    assert_eq!(
        harness.ledger().iter().filter(|run| run.stage == Some(StageId::AggregateVerify)).count(),
        1,
        "final resolution reuses the shared full-suite run instead of dispatching a second suite"
    );
    harness.await_landing(bloom, BloomStatus::Landed);

    for workpiece in [FIRST, SECOND] {
        let workpiece_id = WorkpieceId(workpiece.to_owned());
        assert!(harness.bloom(bloom).has_current_member_resolution(&workpiece_id));
        assert_eq!(
            store.load(&workpiece_id).expect("the commission loads").expect("the member has a commission").head.status,
            CommissionStatus::Landed,
            "landing closes {workpiece}'s contextual commission"
        );
        let claim =
            state.claims.get(workpiece).unwrap_or_else(|| panic!("{workpiece} retains a contextual resolution claim"));
        let ResolutionProof::InComposition { node: proof_node, plan, request, .. } = &claim.proof else {
            panic!("{workpiece} must not receive an invented standalone proof");
        };
        assert_eq!(*proof_node, node.digest(), "{workpiece}'s proof names the executed full node");
        assert_eq!(*plan, dispatch.plan.digest(), "{workpiece}'s proof names the exact contextual plan");
        assert!(
            dispatch.plan.requests.iter().any(|candidate| candidate.digest() == *request),
            "{workpiece}'s proof names one exact request from the contextual plan"
        );
    }
}
