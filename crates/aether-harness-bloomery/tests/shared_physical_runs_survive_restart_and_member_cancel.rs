//! Shared verification crosses the real coordinator, local executor, intake,
//! and durable-store boundaries. These scenarios keep that path honest across
//! the two interruptions that used to lose physical-run progress.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    CoordinationPolicy, MemberVerifyOutcome, ResolutionProof, ResolvedConfigs, SharedRunDispatch, SharedRunMode,
    SharedRunPhase, Snapshot, StageId, VerificationMode, WorkpieceId, decode_recorded_decisions, decode_recorded_event,
};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::{SharedRunLifecycle, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, OperatorMove, Repo};

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

#[test]
fn a_partial_shared_run_restarts_from_its_first_missing_receipt() {
    let roots = HarnessRoots::create();
    let authority = Repo::with_example_project();
    let script = LaneScript::all_passing().then_for(SECOND, StageId::Verify, LaneMode::NeverExits);
    let (bloom, first_verify_runs) = {
        let mut harness = HarnessBuilder::local_authority(&authority)
            .roots(&roots)
            .coordination(warm_serial_policy())
            .script(&script)
            .start("partial-shared-run-before-restart");
        let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
        let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
        let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);

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
    let authority = Repo::with_example_project();
    let script = LaneScript::all_passing().then_for(SECOND, StageId::Verify, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(warm_serial_policy())
        .script(&script)
        .start("cancel-one-shared-member");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);

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
    let authority = Repo::with_example_project();
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        .script(&LaneScript::all_passing())
        .start("contextual-shared-run");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);

    harness.pump_until("the contextual node becomes the resolved full root", |harness| {
        let snapshot = replay_snapshot(&mut harness.commission_store());
        snapshot.blooms.get(&bloom).is_some_and(|record| {
            let Some(state) = record.coordination.as_ref() else {
                return false;
            };
            state.integration.head.coverage.len() == 2
                && state.claims.len() == 2
                && state.contextual_aggregate_proof(&state.integration.head).is_some()
                && record.integration.as_ref().is_some_and(|root| {
                    root.tree == state.integration.head.candidate.tree
                        && root.head == state.integration.head.candidate.checkout
                })
        })
    });

    let mut store = harness.commission_store();
    let (physical, dispatch) = store
        .list_shared_runs()
        .expect("shared runs read after contextual completion")
        .into_iter()
        .find_map(|run| {
            let dispatch = from_bytes::<SharedRunDispatch>(&run.dispatch).ok()?;
            let members =
                dispatch.plan.requests.iter().map(|request| request.member.workpiece.0.as_str()).collect::<Vec<_>>();
            (run.lifecycle == SharedRunLifecycle::Completed
                && dispatch.plan.mode == SharedRunMode::Contextual
                && members == [FIRST, SECOND])
            .then_some((run, dispatch))
        })
        .expect("A and B execute together in one completed contextual physical run");
    let steps = store.shared_run_steps(&physical.run).expect("contextual physical steps read");
    assert!(!steps.is_empty(), "the contextual physical run records its execution step");
    assert!(steps.iter().all(|step| step.receipt.is_some()), "every contextual execution step has settled");

    let snapshot = replay_snapshot(&mut store);
    let record = snapshot.blooms.get(&bloom).expect("the contextual bloom remains projected");
    let state = record.coordination.as_ref().expect("the contextual coordination state remains projected");
    let run = state.run(dispatch.plan.digest()).expect("the executed contextual plan remains projected");
    assert_eq!(run.phase, SharedRunPhase::Terminal, "the contextual logical run reached its terminal phase");
    assert!(run.physical_run.is_some(), "the logical run retains its real physical-run identity");
    assert!(
        run.completed.iter().all(|outcome| matches!(outcome, MemberVerifyOutcome::PassedIn { .. })),
        "the aggregate receipt settles every member in the contextual position"
    );
    let node = run.node.as_ref().expect("the contextual run retains its immutable prepared node");
    assert_eq!(node.plan, dispatch.plan.digest(), "the node is bound to the exact dispatched shared plan");
    assert_eq!(state.integration.head.node, node.digest(), "the selected full root is the executed contextual node");
    assert_eq!(state.integration.head.candidate, node.candidate, "the selected full root retains the prepared tree");
    assert_eq!(state.integration.head.coverage, node.coverage, "the selected full root retains exact member coverage");

    for workpiece in [FIRST, SECOND] {
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
