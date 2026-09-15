//! Ready contextual verifies wait for sibling constructs still in flight.
//!
//! Sequential construct completions used to each start their own shared run
//! the moment a prover was free. A coalescing hold keeps those requests
//! together so members in one run integrate together and never displace each
//! other.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    CoordinationPolicy, Digest, Fact, FakeKeyProvider, KeyId, SharedRunDispatch, StageId, VerificationMode,
    decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";
const THIRD: &str = "wp-c";

const MEMBERS: [(&str, &str, &str); 3] = [
    (FIRST, "crates/example-a/**", "crates/example-a/src/contextual.rs"),
    (SECOND, "crates/example-b/**", "crates/example-b/src/contextual.rs"),
    (THIRD, "crates/example-shared/**", "crates/example-shared/src/contextual.rs"),
];

fn coalescing_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: true,
        max_run_members: 3,
        max_serial_requests: 3,
        max_attribution_probes: 3,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn immediate_policy() -> CoordinationPolicy {
    CoordinationPolicy { coalesce_millis: Some(0), ..coalescing_policy() }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("coalesce harness")), &[0x0A; 32], *scope_revision);
        store
            .insert_approval(&approval, &FakeKeyProvider)
            .expect("the original member scope retains its signed approval");
    }
}

fn park_constructs(harness: &mut ScenarioHarness) -> Vec<OutstandingOrder> {
    harness.pump_until("all three members receive their parked Construct orders", |harness| {
        let orders = harness.orders();
        assert!(orders.len() <= 3, "only the three sealed members can be dispatched: {orders:?}");
        orders.len() == 3
    });
    let constructs = harness.orders();
    assert!(
        constructs
            .iter()
            .all(|order| from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::Construct)),
        "the barrier holds exactly the three Construct orders: {constructs:?}"
    );
    harness.pump_until("all parked Construct children reached their real worktrees", |harness| {
        let runs = harness.ledger();
        constructs.iter().all(|order| runs.iter().any(|run| run.nonce == order.nonce))
    });
    let runs = harness.ledger();
    for (workpiece, _, path) in MEMBERS {
        let order = constructs.iter().find(|order| order.workpiece == workpiece).expect("sealed member Construct");
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
    constructs
}

fn release_construct(harness: &mut ScenarioHarness, constructs: &[OutstandingOrder], workpiece: &str) {
    let order = constructs.iter().find(|order| order.workpiece == workpiece).expect("member Construct order");
    harness.release_parked_lanes(from_ref(order));
}

fn queued_verifies(harness: &ScenarioHarness) -> usize {
    harness.commission_store().queued_member_verifications().expect("logical verification requests read").len()
}

fn shared_run_members(harness: &ScenarioHarness) -> Vec<Vec<String>> {
    harness
        .commission_store()
        .list_shared_runs()
        .expect("shared runs read")
        .iter()
        .filter_map(|run| from_bytes::<SharedRunDispatch>(&run.dispatch).ok())
        .map(|dispatch| dispatch.plan.requests.iter().map(|request| request.member.workpiece.0.clone()).collect())
        .collect()
}

fn hold_facts(harness: &ScenarioHarness) -> usize {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .iter()
        .filter_map(|row| decode_recorded_event(&row.event, row.event_schema.as_deref()).ok())
        .filter(|event| matches!(event.fact, Fact::HoldSharedRunCoalesce { .. }))
        .count()
}

fn integration_count(harness: &ScenarioHarness) -> usize {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .iter()
        .filter_map(|row| decode_recorded_event(&row.event, row.event_schema.as_deref()).ok())
        .filter(|event| matches!(event.fact, Fact::IntegrationAdvanced { .. }))
        .count()
}

fn seal_three(harness: &mut ScenarioHarness) {
    let scopes = MEMBERS
        .iter()
        .map(|(workpiece, surface, _)| (*workpiece, harness.author_scope_revision(workpiece, from_ref(surface))))
        .collect::<Vec<_>>();
    approve_scopes(harness, &scopes.iter().map(|(_, revision)| *revision).collect::<Vec<_>>());
    let sealed = scopes.iter().map(|(workpiece, revision)| (*workpiece, *revision)).collect::<Vec<_>>();
    let _ = harness.seal_members(&sealed);
}

#[test]
fn three_ready_siblings_share_one_run_when_the_hold_covers_the_spread() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FIRST, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Construct, LaneMode::NeverExits)
        .then_for(THIRD, StageId::Construct, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(coalescing_policy())
        .max_concurrent_provers(1)
        .script(&script)
        .start("ready-siblings-share-one-run");
    seal_three(&mut harness);
    let constructs = park_constructs(&mut harness);

    release_construct(&mut harness, &constructs, FIRST);
    // The hold is journaled by the scheduler's proposal pass, which is a later
    // tick than the one that queues the request — so it is waited for, not
    // snapshotted after a wait for something else (iamacoffeepot/aether#6078).
    // A coordinator that never holds still fails here, on this budget, naming
    // the wait it did not journal.
    harness.pump_until("the first candidate queues and its wait is journaled", |harness| {
        queued_verifies(harness) == 1 && hold_facts(harness) >= 1
    });
    assert!(shared_run_members(&harness).is_empty(), "a sibling construct in flight must hold the first request");

    release_construct(&mut harness, &constructs, SECOND);
    harness.pump_until("the second candidate joins the held queue", |harness| queued_verifies(harness) == 2);
    assert!(shared_run_members(&harness).is_empty(), "two ready siblings still wait for the third construct");

    release_construct(&mut harness, &constructs, THIRD);
    harness.pump_until("one shared-run plan carries every ready sibling", |harness| {
        shared_run_members(harness).iter().any(|members| members.len() == 3)
    });
    let plans = shared_run_members(&harness);
    assert_eq!(plans.len(), 1, "the hold must not start a second plan: {plans:?}");
    let mut members = plans[0].clone();
    members.sort();
    assert_eq!(members, [FIRST, SECOND, THIRD].map(str::to_owned));

    harness.pump_until("the coalesced run integrates once", |harness| integration_count(harness) == 1);
    assert_eq!(integration_count(&harness), 1, "members in one run integrate together");
}

#[test]
fn a_zero_hold_keeps_per_member_runs() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FIRST, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Construct, LaneMode::NeverExits)
        .then_for(THIRD, StageId::Construct, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(immediate_policy())
        .max_concurrent_provers(1)
        .script(&script)
        .start("zero-hold-keeps-per-member-runs");
    seal_three(&mut harness);
    let constructs = park_constructs(&mut harness);

    release_construct(&mut harness, &constructs, FIRST);
    harness.pump_until("the first ready request starts its own plan", |harness| {
        shared_run_members(harness).iter().any(|members| members.as_slice() == [FIRST])
    });

    release_construct(&mut harness, &constructs, SECOND);
    harness
        .pump_until("the second ready request starts its own plan", |harness| shared_run_members(harness).len() >= 2);

    release_construct(&mut harness, &constructs, THIRD);
    harness.pump_until("each sibling ran alone", |harness| shared_run_members(harness).len() == 3);

    let mut plans = shared_run_members(&harness);
    plans.sort();
    assert_eq!(
        plans,
        vec![vec![FIRST.to_owned()], vec![SECOND.to_owned()], vec![THIRD.to_owned()]],
        "a zero hold is the old per-member behaviour, not an accident of arrival timing"
    );
    assert!(
        plans.iter().all(|members| members.len() == 1),
        "no plan may pick up a sibling that arrived after it started: {plans:?}"
    );
}
