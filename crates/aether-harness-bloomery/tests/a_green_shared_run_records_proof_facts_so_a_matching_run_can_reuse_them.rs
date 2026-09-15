//! A green contextual shared run must record one proof fact per declared gate
//! so a later request on an untouched closure can reuse them (#5948).
//!
//! Production ran 47 shared runs and wrote zero `proof_facts` rows because
//! discrimination waited for a second independent suite a single green run
//! never supplies. This scenario seals two disjoint members, lets the first
//! green shared run complete, and asserts the ledger holds those facts. After
//! the first member integrates, the second run completes without wiping them,
//! and a later request whose contextual key matches the first run reuses the
//! facts instead of executing the gates again.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    BloomId, CoordinationPolicy, Digest, Fact, FakeKeyProvider, IntegrationHead, KeyId, MemberPin, ResolvedConfigs,
    SharedRunDispatch, SharedRunExecution, SharedRunPlan, Snapshot, StageId, VerificationMode,
    decode_recorded_decisions, decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::bloomery::{HostClass, reuse_contextual_proof};
use aether_chassis_bloomery::store::{CommissionBackend, SharedRunRow, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";

fn eager_contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: true,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 2,
        movement_budget: 2,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("green-proof harness")), &[0x0A; 32], *scope_revision);
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

fn facts<T>(harness: &ScenarioHarness, select: impl Fn(&Fact) -> Option<T>) -> Vec<T> {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .iter()
        .filter_map(|row| decode_recorded_event(&row.event, row.event_schema.as_deref()).ok())
        .filter_map(|event| select(&event.fact))
        .collect()
}

fn head(harness: &ScenarioHarness, bloom: BloomId) -> IntegrationHead {
    let snapshot = replay_snapshot(&mut harness.commission_store());
    snapshot
        .blooms
        .get(&bloom)
        .expect("the sealed bloom projects")
        .coordination
        .as_ref()
        .expect("coordination")
        .integration
        .head
        .clone()
}

fn covers(coverage: &[MemberPin], workpiece: &str) -> bool {
    coverage.iter().any(|pin| pin.workpiece.0 == workpiece)
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
            harness.commission_store().queued_member_verifications().expect("logical verification requests read").len()
                == index + 1
        });
    }
    harness.hold_member_verification(false);
}

fn proposed_plans(harness: &ScenarioHarness) -> Vec<SharedRunPlan> {
    facts(harness, |fact| match fact {
        Fact::ProposeSharedRun { plan, .. } => Some(plan.clone()),
        _ => None,
    })
}

fn completed_plans(harness: &ScenarioHarness) -> Vec<Digest> {
    facts(harness, |fact| match fact {
        Fact::SharedRunCompleted { completion, .. } => Some(completion.plan),
        _ => None,
    })
}

fn first_completed_run(harness: &ScenarioHarness) -> SharedRunRow {
    let completed = completed_plans(harness);
    harness
        .commission_store()
        .list_shared_runs()
        .expect("shared runs read")
        .into_iter()
        .find(|row| {
            from_bytes::<SharedRunDispatch>(&row.dispatch)
                .is_ok_and(|dispatch| completed.contains(&dispatch.plan.digest()))
        })
        .expect("the completed shared run is retained")
}

#[test]
fn a_green_shared_run_records_proof_facts_so_a_matching_run_can_reuse_them() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then_for(FIRST, StageId::Construct, LaneMode::NeverExits).then_for(
        SECOND,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_contextual_policy())
        .script(&script)
        .start("green-shared-run-records-proof-facts");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first, second]);
    let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);
    complete_joint_constructions(&mut harness);

    harness
        .pump_until("both members are proposed before either prepares", |harness| proposed_plans(harness).len() == 2);
    harness.pump_until("a shared run completes green", |harness| !completed_plans(harness).is_empty());

    let first_run = first_completed_run(&harness);
    let first_dispatch = from_bytes::<SharedRunDispatch>(&first_run.dispatch).expect("completed dispatch decodes");
    let declared = first_dispatch
        .plan
        .composition
        .as_ref()
        .map(|composition| composition.contract.gate_identities.clone())
        .expect("a contextual run declares gates");
    assert!(!declared.is_empty(), "the sealed contract names at least one gate");

    let recorded = harness.commission_store().list_proof_facts().expect("the proof ledger reads");
    assert!(
        !recorded.is_empty(),
        "a green shared run must record proof facts; production wrote zero rows after 47 runs"
    );
    assert!(
        recorded.iter().all(|row| row.result == "green" && row.host_class == "harness"),
        "green facts stamp the executor host class: {recorded:?}"
    );
    for gate in &declared {
        assert!(
            recorded.iter().any(|row| row.test_id == format!("gate:{gate}")),
            "declared gate {gate} is missing from {recorded:?}"
        );
    }

    let leading = first_dispatch.plan.requests[0].member.workpiece.0.as_str();
    harness.integrate_tick();
    harness.pump_until("the finished sibling occupies the head", |harness| {
        covers(&head(harness, bloom).coverage, leading)
    });

    harness.pump_until("both members' shared runs complete", |harness| completed_plans(harness).len() >= 2);
    harness.integrate_tick();
    harness.pump_until("both members occupy the moved head", |harness| {
        covers(&head(harness, bloom).coverage, FIRST) && covers(&head(harness, bloom).coverage, SECOND)
    });

    let after_second = harness.commission_store().list_proof_facts().expect("the proof ledger still reads");
    assert!(
        after_second.iter().any(|row| recorded.iter().any(|first| first.sequence == row.sequence)),
        "the second run must not wipe the first run's facts: {after_second:?}"
    );

    let SharedRunExecution::Contextual { node, .. } = &first_dispatch.execution else {
        panic!("the completed run is contextual");
    };
    let reuse = reuse_contextual_proof(
        &mut harness.commission_store(),
        node,
        &first_dispatch.plan.composition.as_ref().expect("composition").contract,
        &HostClass::new("harness"),
    )
    .expect("proof lookup")
    .expect("a later request on the first run's exact input reuses rather than re-executing");
    assert_eq!(
        reuse.facts.iter().map(|fact| fact.gate.as_str()).collect::<Vec<_>>(),
        declared.iter().map(String::as_str).collect::<Vec<_>>(),
        "every declared gate has a green witness"
    );
}
