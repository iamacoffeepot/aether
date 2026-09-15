//! Eager integration (ADR-0218) over the real coordinator: the product is
//! assembled one verified member at a time instead of waiting for the whole
//! bloom, and a fold that collides reconciles only the member that authored the
//! candidate while the product keeps the coverage it already earned.
//!
//! The headline of the eager policy is that the assembled product is durable,
//! and nothing below reads test-side bookkeeping for it: the coverage assertions
//! come off the coordinator's own journal, replayed into a `Snapshot` the way an
//! operator read would, and the fold verdicts are the journaled
//! `Fact::IntegrationAdvanced` / `Fact::IntegrationAppendConflicted` rows. A
//! reducer that assembled the product only at final readiness, or that sent both
//! members of a collision back, passes every unit test in the coordination module
//! and fails here.

use std::slice::from_ref;

use aether_bloomery::{BloomId, BloomStatus, CoordinationPolicy, StageId, Transformation, VerificationMode};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::support::eager::{
    approve_scopes, author, claimed, collisions, coordination, folds, parked_lane_at, product, product_coverage,
    replay_snapshot, stage_of,
};
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";
const SHARED: &str = "crates/example-shared/src/lib.rs";

/// What the first member to finish writes over the seed's `1`.
const LEADING_SHARED: &str = "pub fn shared() -> u8 {\n    11\n}\n";

/// What the second writes over the same line, so the two hunks overlap.
const FOLLOWING_SHARED: &str = "pub fn shared() -> u8 {\n    22\n}\n";

/// Eager integration over ordinary standalone member proofs.
///
/// Standalone rather than contextual verification so each member's pass is its
/// own physical run: the scenario is about the product growing per member, and a
/// composition that verified both at once would hide exactly that.
fn eager_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        verification: VerificationMode::Standalone,
        eager_integration: true,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 0,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn covered(harness: &ScenarioHarness, bloom: BloomId, workpiece: &str) -> bool {
    product_coverage(harness, bloom).iter().any(|pin| pin.workpiece.0 == workpiece)
}

fn sibling_of(workpiece: &str) -> &'static str {
    if workpiece == FIRST {
        SECOND
    } else {
        FIRST
    }
}

/// Seal A and B over the formatted example project under the eager policy.
fn sealed_pair(harness: &mut ScenarioHarness, surfaces: [&str; 2]) -> BloomId {
    let first = harness.author_scope_revision(FIRST, from_ref(&surfaces[0]));
    let second = harness.author_scope_revision(SECOND, from_ref(&surfaces[1]));
    approve_scopes(harness, &[first, second]);
    harness.seal_members(&[(FIRST, first), (SECOND, second)])
}

/// The file each member of the independent pair owns inside its own surface.
fn own_path(workpiece: &str) -> String {
    format!("crates/example-{}/src/eager.rs", workpiece.strip_prefix("wp-").unwrap_or(workpiece))
}

#[test]
fn an_eager_product_grows_per_member_rather_than_waiting_for_the_bloom() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then_for(FIRST, StageId::Construct, LaneMode::NeverExits).then_for(
        SECOND,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_policy())
        .script(&script)
        .start("eager-head-advances-per-member");
    let bloom = sealed_pair(&mut harness, ["crates/example-a/**", "crates/example-b/**"]);

    let leading = parked_lane_at(&mut harness, StageId::Construct);
    let lagging = sibling_of(&leading.workpiece).to_owned();
    author(&harness, &leading, &own_path(&leading.workpiece), "pub const LEADING: u8 = 1;\n");
    harness.release_parked_lanes(from_ref(&leading));

    harness.pump_until("the product absorbs the member that finished first", |harness| {
        covered(harness, bloom, &leading.workpiece)
    });
    let partial = product_coverage(&harness, bloom);
    assert_eq!(partial.len(), 1, "the partial product carries exactly the member that has been verified: {partial:?}");
    assert!(!covered(&harness, bloom, &lagging), "{lagging} has not finished, so its pin cannot be in the product");
    assert!(!claimed(&harness, bloom, &lagging), "the product grew while {lagging} was still unverified");
    assert_eq!(
        folds(&harness, bloom).len(),
        1,
        "the partial fold is journaled on its own rather than folded into the final resolve",
    );

    // Now the second member finishes and the same product grows to cover both.
    let following = parked_lane_at(&mut harness, StageId::Construct);
    assert_eq!(following.workpiece, lagging, "the sibling's author order follows its parked predecessor");
    author(&harness, &following, &own_path(&following.workpiece), "pub const FOLLOWING: u8 = 2;\n");
    harness.release_parked_lanes(from_ref(&following));

    harness.pump_until("the product grows to cover both members", |harness| {
        covered(harness, bloom, FIRST) && covered(harness, bloom, SECOND)
    });
    let folded = folds(&harness, bloom);
    assert_eq!(folded.len(), 2, "one journaled fold per member: {folded:?}");
    assert_eq!(folded[0], partial, "the first fold is the partial product, retained exactly");

    harness.pump_until("the bloom resolves the root its own product selected", |harness| {
        replay_snapshot(&mut harness.commission_store())
            .blooms
            .get(&bloom)
            .is_some_and(|record| record.resolved_head.is_some())
    });
    let snapshot = replay_snapshot(&mut harness.commission_store());
    let record = snapshot.blooms.get(&bloom).expect("the eager bloom remains projected");
    let state = record.coordination.as_ref().expect("the eager coordination state remains projected");
    assert_eq!(
        record.resolved_head,
        Some(state.integration.head.candidate.checkout),
        "final resolution adopts the assembled product rather than re-folding the leaves",
    );
    assert_eq!(record.resolved_tree, Some(state.integration.head.candidate.tree));
    assert_eq!(state.integration.head.coverage.len(), 2, "the resolved product covers every active member");
    assert_eq!(
        harness.ledger().iter().filter(|run| run.stage == Some(StageId::AggregateVerify)).count(),
        1,
        "the product is verified once, not once per partial fold",
    );

    harness.await_landing(bloom, BloomStatus::Landed);
}

#[test]
fn an_eager_fold_collision_reconciles_only_the_member_that_authored_the_candidate() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FIRST, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Construct, LaneMode::NeverExits)
        .then_for(FIRST, StageId::Reconcile, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Reconcile, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_policy())
        .script(&script)
        .start("eager-append-collision");
    let bloom = sealed_pair(&mut harness, ["crates/example-shared/**", "crates/example-shared/**"]);

    // Both members rewrite the same line of the same file, so the second one's
    // fold onto the product the first assembled is a real git collision.
    let leading = parked_lane_at(&mut harness, StageId::Construct);
    let colliding = sibling_of(&leading.workpiece).to_owned();
    author(&harness, &leading, SHARED, LEADING_SHARED);
    harness.release_parked_lanes(from_ref(&leading));
    harness.pump_until("the product absorbs the member that finished first", |harness| {
        covered(harness, bloom, &leading.workpiece)
    });
    let survivor_product = product(&harness, bloom);
    let partial = survivor_product.coverage.clone();

    let following = parked_lane_at(&mut harness, StageId::Construct);
    assert_eq!(following.workpiece, colliding, "the sibling's author order follows its parked predecessor");
    author(&harness, &following, SHARED, FOLLOWING_SHARED);
    harness.release_parked_lanes(from_ref(&following));

    harness.pump_until("the second member's fold collides with the product the first assembled", |harness| {
        !collisions(harness, bloom).is_empty()
    });
    let collided = collisions(&harness, bloom);
    assert_eq!(collided.len(), 1, "one journaled collision: {collided:?}");
    assert_eq!(
        collided[0].iter().map(|pin| pin.workpiece.0.as_str()).collect::<Vec<_>>(),
        vec![colliding.as_str()],
        "the collision names only the input that would not place, not the product it collided with",
    );
    assert_eq!(
        product_coverage(&harness, bloom),
        partial,
        "the surviving coverage is retained: a collision reconciles the collider, not the bloom",
    );

    harness.pump_until("only the collider is sent back", |harness| {
        harness.orders().iter().any(|order| order.workpiece == colliding && stage_of(order) == StageId::Reconcile)
    });
    let orders = harness.orders();
    assert!(
        orders.iter().all(|order| order.workpiece != leading.workpiece),
        "the member already in the product is not re-dispatched: {orders:?}",
    );
    assert!(
        claimed(&harness, bloom, &leading.workpiece),
        "the surviving member keeps the verified claim its sibling's collision did not touch",
    );

    // The collider's lap is seeded from the candidate its own lane produced,
    // stands on the product it collided with, and carries the collision
    // diagnostic. It is a merge, and the member's change is already proved.
    let reconcile = parked_lane_at(&mut harness, StageId::Reconcile);
    assert_eq!(reconcile.workpiece, colliding);
    let transformation: Transformation =
        from_bytes(&reconcile.transformation).expect("a recorded order carries a Transformation");
    assert_eq!(
        transformation.inputs[0], collided[0][0].candidate.tree,
        "the lap is seeded from the candidate the collider's own lane produced",
    );
    assert_eq!(
        transformation.checkout, survivor_product.candidate.checkout,
        "the lap's working tree is the assembled product, not the candidate that would not place",
    );
    assert!(
        transformation.description.is_some_and(|order| order.contains(SHARED)),
        "the lap is handed the conflicting path rather than being asked to guess it",
    );

    author(&harness, &reconcile, SHARED, "pub fn shared() -> u8 {\n    33\n}\n");
    harness.release_parked_lanes(from_ref(&reconcile));

    // The reconciled tree is verified as the member authored it. Merging it onto
    // the product first is the step ADR-0218 §Amendment: eager integration
    // assembles the product removed — the merge is what the lap just did.
    harness.pump_until("the reconciled candidate is queued for verification", |harness| {
        coordination(harness, bloom).requests.iter().any(|request| request.member.workpiece.0 == colliding)
    });
    let state = coordination(&harness, bloom);
    assert!(state.prepared.is_empty(), "no candidate is merged onto the product before its own proof: {state:?}");
    assert!(state.preparations.is_empty(), "and none is queued to be");
    let reproved = state
        .requests
        .iter()
        .rev()
        .find(|request| request.member.workpiece.0 == colliding)
        .expect("the collider's confirming request");
    assert_ne!(
        reproved.member.candidate.tree, collided[0][0].candidate.tree,
        "the request proves the tree the lap authored, not the one that would not place",
    );
    assert!(
        claimed(&harness, bloom, &leading.workpiece),
        "the surviving member's verified claim outlives its sibling's reconcile lap",
    );

    // That reconciled tree already carries the survivor's work, so its fold
    // places on the very product the collision refused.
    harness.pump_until("the reconciled contribution joins the product the survivor already holds", |harness| {
        covered(harness, bloom, FIRST) && covered(harness, bloom, SECOND)
    });
    let folded = folds(&harness, bloom);
    assert_eq!(folded.len(), 2, "the reconciled member folds onto the partial product: {folded:?}");
    assert_eq!(folded[0], partial, "the survivor's fold is retained exactly, not rebuilt");
    assert_eq!(
        collisions(&harness, bloom).len(),
        1,
        "the reconciled tree places rather than colliding again on the product it was authored over",
    );

    harness.pump_until("the bloom resolves the root the reconciled product selected", |harness| {
        replay_snapshot(&mut harness.commission_store())
            .blooms
            .get(&bloom)
            .is_some_and(|record| record.resolved_head.is_some())
    });
    let snapshot = replay_snapshot(&mut harness.commission_store());
    let record = snapshot.blooms.get(&bloom).expect("the eager bloom remains projected");
    let state = record.coordination.as_ref().expect("the eager coordination state remains projected");
    assert_eq!(
        record.resolved_head,
        Some(state.integration.head.candidate.checkout),
        "resolution adopts the product the reconciled member rejoined rather than re-folding the leaves",
    );
    assert_eq!(
        harness.ledger().iter().filter(|run| run.stage == Some(StageId::AggregateVerify)).count(),
        1,
        "the root the collision-free product selected is verified once",
    );

    harness.await_landing(bloom, BloomStatus::Landed);
}
