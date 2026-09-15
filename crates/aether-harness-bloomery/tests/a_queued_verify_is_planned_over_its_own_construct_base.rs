//! A member whose verification is still queued when a sibling folds is planned
//! over the base it was **constructed** on, never over the product the fold
//! moved (ADR-0218 §Amendment: a member is verified over the context it was
//! built on).
//!
//! Both members are admitted on the pristine base, so both authored against a
//! tree carrying neither sibling. `wp-a` is released first: it verifies, folds,
//! and the product now carries a file `wp-b`'s candidate has never seen. Only
//! then is `wp-b`'s candidate captured and its run planned.
//!
//! Before this change the plan's `CompositionPlan.base` was
//! `state.integration.head` — the product — so `wp-b` was verified over a tree
//! it never compiled against. Bloom `9680c483` is what that cost: every run
//! rebuilt the whole downstream closure because the tree under test had moved at
//! the previous fold, and two members went red on struct fields a folded sibling
//! had added. The shape is reproduced here by giving `wp-a` a file whose content
//! `wp-b` would have to have been written against, and asserting that the node
//! `wp-b` is proved over does not contain it.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    BloomId, CoordinationPolicy, Digest, Fact, FakeKeyProvider, IntegrationHead, KeyId, MemberPin, ResolvedConfigs,
    SharedRunNode, SharedRunPlan, Snapshot, StageId, VerificationMode, decode_recorded_decisions,
    decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";
const FIRST_FILE: &str = "crates/example-a/src/contextual.rs";
const SECOND_FILE: &str = "crates/example-b/src/contextual.rs";

/// What `wp-a` folds into the product. The `required` field is the shape the
/// evidence names: a sibling's fold adding a field every constructor has to set,
/// which anything built before the fold does not set and cannot compile against.
const FIRST_CONTENT: &str = "pub struct Folded {\n    pub required: bool,\n}\n";

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
        // A sibling construct is deliberately still in flight for most of this
        // scenario, and #6027's coalesce hold would park every ready request
        // behind it. The subject here is the base a plan stands on, not when it
        // is proposed, so the wait is switched off.
        coalesce_millis: Some(0),
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("own-base harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
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
    replay_snapshot(&mut harness.commission_store())
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

fn proposed_plans(harness: &ScenarioHarness) -> Vec<SharedRunPlan> {
    facts(harness, |fact| match fact {
        Fact::ProposeSharedRun { plan, .. } => Some(plan.clone()),
        _ => None,
    })
}

fn plan_for(harness: &ScenarioHarness, workpiece: &str) -> Option<SharedRunPlan> {
    proposed_plans(harness)
        .into_iter()
        .find(|plan| plan.requests.iter().any(|request| request.member.workpiece.0 == workpiece))
}

fn prepared_node(harness: &ScenarioHarness, plan: &SharedRunPlan) -> Option<SharedRunNode> {
    facts(harness, |fact| match fact {
        Fact::SharedRunPrepared { preparation: aether_bloomery::SharedRunPreparation::Contextual(node), .. }
            if node.plan == plan.digest() =>
        {
            Some(node.clone())
        }
        _ => None,
    })
    .pop()
}

/// Park both Construct orders so both members are admitted on the pristine base,
/// then hand each its own approved-surface file without releasing either lane.
fn park_both_constructions(harness: &mut ScenarioHarness) -> Vec<OutstandingOrder> {
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
    for (workpiece, path, content) in [
        (FIRST, FIRST_FILE, FIRST_CONTENT.to_owned()),
        (SECOND, SECOND_FILE, format!("pub const MEMBER: &str = \"{SECOND}\";\n")),
    ] {
        let order =
            constructs.iter().find(|order| order.workpiece == workpiece).expect("the admitted member has an order");
        let worktree = runs
            .iter()
            .find(|run| run.nonce == order.nonce)
            .and_then(|run| run.worktree.as_deref())
            .map(Path::new)
            .expect("the parked local Construct recorded its real worktree");
        fs::remove_file(worktree.join(CANDIDATE_FILE)).expect("the mock's generic candidate is removed");
        fs::write(worktree.join(path), content).expect("the parked Construct writes its own approved surface");
    }
    constructs
}

fn release(harness: &mut ScenarioHarness, constructs: &[OutstandingOrder], workpiece: &str) {
    let order = constructs.iter().find(|order| order.workpiece == workpiece).expect("member Construct order");
    harness.release_parked_lanes(from_ref(order));
}

#[test]
fn a_queued_verify_is_planned_over_its_own_construct_base() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then_for(FIRST, StageId::Construct, LaneMode::NeverExits).then_for(
        SECOND,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_contextual_policy())
        .script(&script)
        .start("queued-verify-is-planned-over-its-own-construct-base");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first, second]);
    let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);

    let constructs = park_both_constructions(&mut harness);
    let construct_base = {
        let snapshot = replay_snapshot(&mut harness.commission_store());
        let state = snapshot.blooms.get(&bloom).expect("bloom").coordination.as_ref().expect("coordination").clone();
        let context = state.contexts.get(SECOND).expect("the second member was admitted with a context").clone();
        assert!(context.starting_head.coverage.is_empty(), "both members were admitted on the pristine base");
        context.starting_head
    };

    release(&mut harness, &constructs, FIRST);
    harness.pump_until("the first member's candidate is captured", |harness| {
        harness.commission_store().queued_member_verifications().expect("the verification queue reads").len() == 1
    });
    harness.hold_member_verification(false);
    harness.pump_until("the first member verifies and occupies the product", |harness| {
        harness.dispatch_tick();
        harness.integrate_tick();
        covers(&head(harness, bloom).coverage, FIRST)
    });
    let moved = head(&harness, bloom);
    assert_ne!(moved, construct_base, "the product moved out from under the member still in construction");

    release(&mut harness, &constructs, SECOND);
    harness.pump_until("the lagging member is planned", |harness| plan_for(harness, SECOND).is_some());
    let plan = plan_for(&harness, SECOND).expect("the lagging member was planned");
    let composition = plan.composition.as_ref().expect("a contextual plan carries its composition");
    let request = plan.requests.first().expect("the plan carries its member's request");

    assert_eq!(
        composition.base, construct_base,
        "the plan stands on the base the member was constructed on, not on the product: {composition:?}"
    );
    assert!(
        !covers(&composition.base.coverage, FIRST),
        "the folded sibling is not part of the tree this member is judged on"
    );
    assert_eq!(
        request.context.as_ref().map(|context| context.starting_head.clone()),
        Some(construct_base),
        "the request names the same base the plan composes over"
    );
    assert_eq!(
        request.input.candidate, request.member.candidate,
        "the input is the member's own candidate over its construct base"
    );

    harness.pump_until("the lagging plan prepares its node", |harness| {
        harness.dispatch_tick();
        prepared_node(harness, &plan).is_some()
    });
    let node = prepared_node(&harness, &plan).expect("the lagging plan prepared its own node");
    assert_eq!(
        node.candidate, request.member.candidate,
        "a fold onto the member's own base fast-forwards, so the lane is handed the candidate's own checkout"
    );
    assert!(!covers(&node.coverage, FIRST), "the proved node carries only the member that authored it");
    assert_ne!(
        node.candidate.tree, moved.candidate.tree,
        "the tree under test is the member's own, not the product the sibling's fold produced"
    );

    let first_plans = proposed_plans(&harness)
        .into_iter()
        .filter(|other| other.requests.iter().any(|request| request.member.workpiece.0 == FIRST))
        .collect::<Vec<_>>();
    assert_eq!(
        first_plans.len(),
        1,
        "the folded sibling is proposed once and never re-verified: it is green and it is done: {first_plans:?}"
    );

    harness.pump_until("the lagging member's own proof folds onto the product", |harness| {
        harness.dispatch_tick();
        harness.integrate_tick();
        covers(&head(harness, bloom).coverage, SECOND)
    });
    let settled = head(&harness, bloom);
    assert!(
        covers(&settled.coverage, FIRST) && covers(&settled.coverage, SECOND),
        "the product carries both members once the second one's own proof folds: {settled:?}"
    );
    assert_eq!(
        facts(&harness, |fact| match fact {
            Fact::SharedRunCompleted { completion, .. } => Some(completion.plan),
            _ => None,
        })
        .len(),
        2,
        "each member's work was proved exactly once: a fold re-verifies nobody"
    );
}
