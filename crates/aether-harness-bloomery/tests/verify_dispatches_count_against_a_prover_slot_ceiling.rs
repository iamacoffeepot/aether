//! Model lanes and `verify.*` dispatches occupy separate host ceilings.
//!
//! A single occupancy pool made every ready prove start the moment a slot
//! freed, so ADR-0218 coalescing never saw a queue, and a running rustc
//! refused a construct a slot. These scenarios seal more members than prover
//! slots and assert both halves of that split.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    CoordinationPolicy, Digest, FakeKeyProvider, KeyId, SharedRunDispatch, StageId, VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";

fn contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: false,
        max_run_members: 2,
        max_serial_requests: 2,
        max_attribution_probes: 2,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn standalone_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Standalone,
        eager_integration: false,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 0,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("prover-slot harness")), &[0x0A; 32], *scope_revision);
        store
            .insert_approval(&approval, &FakeKeyProvider)
            .expect("the original member scope retains its signed approval");
    }
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
            let mut store = harness.commission_store();
            assert!(store.list_shared_runs().expect("shared runs read while service is held").is_empty());
            store.queued_member_verifications().expect("logical verification requests read").len() == index + 1
        });
    }
    harness.hold_member_verification(false);
}

#[test]
fn ready_members_share_one_prover_slot() {
    // Two members ready together under one prove slot. If verifies still
    // counted against the model-lane ceiling (default 3), each would start a
    // one-member plan the moment it queued. Scarce provers plus existing
    // coalescing put both requests on one shared-run plan.
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then_for(FIRST, StageId::Construct, LaneMode::NeverExits).then_for(
        SECOND,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        .max_concurrent_provers(1)
        .script(&script)
        .start("ready-members-share-one-prover-slot");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first, second]);
    let _bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);
    complete_joint_constructions(&mut harness);

    harness.pump_until("one shared-run plan carries both ready members", |harness| {
        !harness.commission_store().list_shared_runs().expect("shared runs read after the hold lifts").is_empty()
    });
    let runs = harness.commission_store().list_shared_runs().expect("shared runs read");
    let plans: Vec<Vec<String>> = runs
        .iter()
        .filter_map(|run| from_bytes::<SharedRunDispatch>(&run.dispatch).ok())
        .map(|dispatch| dispatch.plan.requests.iter().map(|request| request.member.workpiece.0.clone()).collect())
        .collect();
    assert_eq!(plans.len(), 1, "scarce provers must not start a second plan: {plans:?}");
    assert_eq!(plans[0].len(), 2, "the one plan carries both ready members: {plans:?}");
}

#[test]
fn a_model_lane_starts_while_a_prover_slot_is_held() {
    // One model slot and one prove slot. The first construct occupies the
    // model slot; the second queues. Completing the first frees the model slot
    // and wants a prove. A shared pool of one would hand that slot to the
    // verify (Judge over Start) and leave the second construct queued. Split
    // pools start both.
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FIRST, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Construct, LaneMode::NeverExits)
        .then_for(FIRST, StageId::Verify, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Verify, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(standalone_policy())
        .max_concurrent_lanes(1)
        .max_concurrent_provers(1)
        .script(&script)
        .start("a-model-lane-starts-while-a-prover-slot-is-held");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first, second]);
    let _bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);

    harness.pump_until("one Construct occupies the only model slot", |harness| {
        harness.ledger().iter().filter(|run| run.stage == Some(StageId::Construct)).count() == 1
    });
    let leading = harness
        .orders()
        .into_iter()
        .find(|order| {
            from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::Construct)
                && harness.ledger().iter().any(|run| run.nonce == order.nonce)
        })
        .expect("the running Construct is an outstanding order");
    let path = if leading.workpiece == FIRST {
        "crates/example-a/src/contextual.rs"
    } else {
        "crates/example-b/src/contextual.rs"
    };
    harness.pump_until("the leading Construct recorded its worktree", |harness| {
        harness.ledger().iter().any(|run| run.nonce == leading.nonce && run.worktree.is_some())
    });
    let worktree = harness
        .ledger()
        .iter()
        .find(|run| run.nonce == leading.nonce)
        .and_then(|run| run.worktree.clone())
        .expect("the leading Construct recorded its real worktree");
    let worktree = Path::new(&worktree);
    fs::remove_file(worktree.join(CANDIDATE_FILE)).expect("the mock's generic candidate is removed");
    let workpiece = leading.workpiece.as_str();
    fs::write(worktree.join(path), format!("pub const MEMBER: &str = \"{workpiece}\";\n"))
        .expect("the parked Construct writes its own approved surface");
    harness.release_parked_lanes(from_ref(&leading));

    harness.pump_until("the trailing Construct starts beside the leading member's prove", |harness| {
        let runs = harness.ledger();
        let trailing_construct = runs.iter().any(|run| {
            run.stage == Some(StageId::Construct) && run.workpiece.as_deref() != Some(leading.workpiece.as_str())
        });
        let leading_verify = runs
            .iter()
            .any(|run| run.stage == Some(StageId::Verify) && run.workpiece.as_deref() == Some(&leading.workpiece));
        trailing_construct && leading_verify
    });
}
