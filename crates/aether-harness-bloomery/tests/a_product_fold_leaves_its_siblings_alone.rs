//! Three members, one product, and no member told that it grew (ADR-0218
//! §Amendment: eager integration assembles the product).
//!
//! This is issue-5978's morning, replayed. A finishes and folds into the product
//! while B and C are still authoring on the tree they were handed. Under the
//! conception this scenario exists to forbid, that fold reached them: their
//! contexts were re-pinned onto the larger tree, and each candidate then owed a
//! merge onto it before anything would judge it — which is how issue-5978 bought
//! a 15-minute reconcile lap and a 60-minute verify to learn that its siblings
//! had landed first. Here B and C are left alone, and each is judged on the tree
//! it actually authored.
//!
//! B's change is independent, so B's proof stands and its fold is clean: no
//! Reconcile at all. C rewrites the same line A did, so C's fold really does
//! collide — and a collision is a merge, it belongs to the one member that
//! authored the candidate, and it reaches C only, *after* C is green, which is
//! what makes the lap a merge rather than a re-authoring.
//!
//! Everything asserted below comes off the coordinator's own journal, replayed
//! the way an operator read would.

use aether_bloomery::{BloomId, BloomStatus, ConstructContext, CoordinationPolicy, StageId, VerificationMode};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_harness_bloomery::support::eager::{
    approve_scopes, author_and_release, claimed, collisions, coordination, folds, parked_lane, product_coverage,
    replay_snapshot, stage_of,
};
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FOLDING: &str = "wp-a";
const INDEPENDENT: &str = "wp-b";
const COLLIDING: &str = "wp-c";

const SHARED: &str = "crates/example-shared/src/lib.rs";
const OWN: &str = "crates/example-b/src/lib.rs";

/// Eager integration over standalone member proofs, so each member's pass is its
/// own physical run and the product grows one member at a time.
///
/// The coalescing hold is off: this scenario deliberately proves one member
/// while its siblings are still authoring, and the default hold would make the
/// scheduler wait for them.
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
        coalesce_millis: Some(0),
    }
}

fn covered(harness: &ScenarioHarness, bloom: BloomId, workpiece: &str) -> bool {
    product_coverage(harness, bloom).iter().any(|pin| pin.workpiece.0 == workpiece)
}

fn orders_for(orders: &[OutstandingOrder], workpiece: &str) -> Vec<String> {
    orders.iter().filter(|order| order.workpiece == workpiece).map(|order| order.nonce.clone()).collect()
}

fn context_of(harness: &ScenarioHarness, bloom: BloomId, workpiece: &str) -> ConstructContext {
    coordination(harness, bloom).contexts.remove(workpiece).expect("an admitted author carries its context")
}

fn reconciled_members(harness: &ScenarioHarness) -> Vec<String> {
    harness
        .ledger()
        .iter()
        .filter(|run| run.stage == Some(StageId::Reconcile))
        .filter_map(|run| run.workpiece.clone())
        .collect()
}

fn seal_three(harness: &mut ScenarioHarness) -> BloomId {
    let folding = harness.author_scope_revision(FOLDING, &["crates/example-shared/**"]);
    let independent = harness.author_scope_revision(INDEPENDENT, &["crates/example-b/**"]);
    let colliding = harness.author_scope_revision(COLLIDING, &["crates/example-shared/**"]);
    approve_scopes(harness, &[folding, independent, colliding]);
    harness.seal_members(&[(FOLDING, folding), (INDEPENDENT, independent), (COLLIDING, colliding)])
}

#[test]
fn a_product_fold_leaves_its_siblings_alone_and_reconciles_only_the_collider() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FOLDING, StageId::Construct, LaneMode::NeverExits)
        .then_for(INDEPENDENT, StageId::Construct, LaneMode::NeverExits)
        .then_for(COLLIDING, StageId::Construct, LaneMode::NeverExits)
        .then_for(COLLIDING, StageId::Reconcile, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_policy())
        .script(&script)
        .max_concurrent_lanes(4)
        .start("product-fold-leaves-siblings-alone");
    let bloom = seal_three(&mut harness);

    // All three are authoring at once, each on the empty product the seal handed
    // them.
    let construct_a = parked_lane(&mut harness, FOLDING, StageId::Construct);
    let construct_b = parked_lane(&mut harness, INDEPENDENT, StageId::Construct);
    let construct_c = parked_lane(&mut harness, COLLIDING, StageId::Construct);
    let handed_to_b = context_of(&harness, bloom, INDEPENDENT);
    let handed_to_c = context_of(&harness, bloom, COLLIDING);

    // A finishes first and the product absorbs it.
    author_and_release(&mut harness, &construct_a, SHARED, "pub fn shared() -> u8 {\n    11\n}\n");
    harness
        .pump_until("the product absorbs the member that finished first", |harness| covered(harness, bloom, FOLDING));
    assert_eq!(product_coverage(&harness, bloom).len(), 1, "the product carries exactly the member that is proved");

    // The fold is not news the authors are told. Neither is re-pinned onto the
    // larger tree, neither owes it a merge, and neither is re-dispatched.
    assert_eq!(context_of(&harness, bloom, INDEPENDENT), handed_to_b, "an author is never re-pinned under its lane");
    assert_eq!(context_of(&harness, bloom, COLLIDING), handed_to_c, "and neither is its sibling");
    let live = harness.orders();
    assert_eq!(orders_for(&live, INDEPENDENT), vec![construct_b.nonce.clone()], "B keeps the one order it was given");
    assert_eq!(orders_for(&live, COLLIDING), vec![construct_c.nonce.clone()], "and so does C");
    let state = coordination(&harness, bloom);
    assert!(state.preparations.is_empty() && state.prepared.is_empty(), "no candidate owes the product a merge");

    // B's change is independent, so B is judged on the tree it authored and its
    // fold is clean.
    author_and_release(&mut harness, &construct_b, OWN, "pub fn b() -> u8 {\n    22\n}\n");
    harness.pump_until("the product grows to carry the independent sibling", |harness| {
        covered(harness, bloom, INDEPENDENT)
    });
    assert_eq!(folds(&harness, bloom).len(), 2, "one journaled fold per proved member");
    assert!(collisions(&harness, bloom).is_empty(), "an independent contribution folds without a collision");
    assert!(reconciled_members(&harness).is_empty(), "and without a Reconcile: it was proved as authored");

    // C rewrote the same line A did, so C's fold is a real git collision.
    author_and_release(&mut harness, &construct_c, SHARED, "pub fn shared() -> u8 {\n    33\n}\n");
    harness.pump_until("C's fold collides with the product its siblings assembled", |harness| {
        !collisions(harness, bloom).is_empty()
    });

    let collided = collisions(&harness, bloom);
    assert_eq!(collided.len(), 1, "one journaled collision: {collided:?}");
    assert_eq!(
        collided[0].iter().map(|pin| pin.workpiece.0.as_str()).collect::<Vec<_>>(),
        vec![COLLIDING],
        "the collision names only the candidate that would not place",
    );
    assert!(
        covered(&harness, bloom, FOLDING) && covered(&harness, bloom, INDEPENDENT),
        "the coverage the product already earned survives the collision",
    );
    assert!(
        claimed(&harness, bloom, FOLDING) && claimed(&harness, bloom, INDEPENDENT),
        "and so do the proofs behind it",
    );

    // Only C is sent back, and its lap is a merge over work that is already
    // green: C passed its own Verify before it was ever offered to the product.
    let reconcile = parked_lane(&mut harness, COLLIDING, StageId::Reconcile);
    let live = harness.orders();
    assert!(
        orders_for(&live, FOLDING).is_empty() && orders_for(&live, INDEPENDENT).is_empty(),
        "the members already in the product are not re-dispatched: {live:?}",
    );
    assert!(claimed(&harness, bloom, COLLIDING), "the collider proved its own candidate before the collision");

    author_and_release(&mut harness, &reconcile, SHARED, "pub fn shared() -> u8 {\n    44\n}\n");
    harness.pump_until("the reconciled contribution joins the product", |harness| covered(harness, bloom, COLLIDING));
    let state = coordination(&harness, bloom);
    assert!(
        state.prepared.is_empty() && state.preparations.is_empty(),
        "the reconciled tree was verified as authored, not merged onto the product first",
    );
    assert_eq!(collisions(&harness, bloom).len(), 1, "the reconciled tree places rather than colliding again");
    assert_eq!(reconciled_members(&harness), vec![COLLIDING], "no member other than the collider ever reconciled");

    // The product that lands is the one the folds assembled, and it carries its
    // own aggregate proof.
    harness.pump_until("the bloom resolves the product its own folds assembled", |harness| {
        replay_snapshot(&mut harness.commission_store())
            .blooms
            .get(&bloom)
            .is_some_and(|record| record.resolved_head.is_some())
    });
    let snapshot = replay_snapshot(&mut harness.commission_store());
    let record = snapshot.blooms.get(&bloom).expect("the eager bloom remains projected");
    let resolved = record.coordination.as_ref().expect("the eager coordination state remains projected");
    assert_eq!(record.resolved_head, Some(resolved.integration.head.candidate.checkout));
    assert_eq!(resolved.integration.head.coverage.len(), 3, "the resolved product covers every active member");
    assert_eq!(
        harness.ledger().iter().filter(|run| run.stage == Some(StageId::AggregateVerify)).count(),
        1,
        "the assembled product is proved, once",
    );
    assert!(
        harness.orders().iter().all(|order| stage_of(order) != StageId::Verify),
        "nothing is left verifying once the product is proved",
    );

    harness.await_landing(bloom, BloomStatus::Landed);
}
