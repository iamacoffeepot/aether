//! A construction still sitting in the queue when the product head advances is
//! admitted on the context it was handed, not retired (issue 6051).
//!
//! This is 2026-09-15's retrospect bloom, shrunk to three members and one lane.
//! The lane cap drains `queued_construction` one member at a time, so a member
//! at the back of the queue waits through however many folds land while the ones
//! ahead of it author and prove. Each queued dispatch carries the
//! `ConstructContext` captured when it was queued, and under the conception this
//! scenario exists to forbid, admission demanded that context still name the
//! *current* `integration.head`: the first fold moved the head, the queued
//! dispatch was answered `InvalidPlan`, the host retired the admission and acked
//! its topic row, and the member sat in Construct with no order, no wedge and no
//! diagnostic until someone read the journal by hand. Four members went that way
//! on the live coordinator.
//!
//! The window is not narrow and re-queueing does not close it: the product folds
//! every few minutes and the admission a re-queue mints is confirmed a step
//! later, so the head moves between the queue row and its confirmation just as
//! readily. What makes a queued construction safe is the same rule that makes a
//! fold invisible to a running lane — a member proves the candidate it authored
//! over the context it was built on, and the merge onto whatever the product has
//! become is a later, separate lap (ADR-0218 §Amendment). So the context the
//! dispatch carries is the context it is admitted on, however far the product
//! has moved since.
//!
//! Everything asserted below comes off the coordinator's own journal, replayed
//! the way an operator read would.

use std::slice::from_ref;

use aether_bloomery::{BloomId, BloomStatus, CoordinationPolicy, StageId, VerificationMode};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_harness_bloomery::support::eager::{
    approve_scopes, author_and_release, coordination, covers, parked_lane_at, product_coverage, replay_snapshot,
    stage_of,
};
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const MEMBERS: [(&str, &str, &str); 3] = [
    ("wp-a", "crates/example-a/**", "crates/example-a/src/queued.rs"),
    ("wp-b", "crates/example-b/**", "crates/example-b/src/queued.rs"),
    ("wp-c", "crates/example-shared/**", "crates/example-shared/src/queued.rs"),
];

/// Eager integration over standalone member proofs, so each pass is its own
/// physical run and the product grows one member at a time — the head moves
/// under whatever is still queued, which is the whole subject here. The
/// admission seam this exercises is upstream of the verification mode: every
/// Construct dispatch is rewritten into a queued admission whatever the bloom
/// proves with.
///
/// The coalescing hold is off, because this scenario deliberately proves one
/// member while its siblings have not started, and the default hold would make
/// the scheduler wait for them.
fn eager_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
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

fn candidate_path(workpiece: &str) -> &'static str {
    MEMBERS.iter().find(|(name, _, _)| *name == workpiece).expect("a sealed member").2
}

fn seal_three(harness: &mut ScenarioHarness) -> BloomId {
    let revisions = MEMBERS
        .iter()
        .map(|(workpiece, surface, _)| (*workpiece, harness.author_scope_revision(workpiece, &[surface])))
        .collect::<Vec<_>>();
    approve_scopes(harness, &revisions.iter().map(|(_, revision)| *revision).collect::<Vec<_>>());
    harness.seal_members(&revisions)
}

/// Every member that has ever been handed a Construct lane, in journal order.
fn constructing_members(harness: &ScenarioHarness) -> Vec<String> {
    harness
        .ledger()
        .iter()
        .filter(|run| run.stage == Some(StageId::Construct))
        .filter_map(|run| run.workpiece.clone())
        .collect()
}

/// Wait for the parked Construct order of whichever member has not had one yet.
fn next_parked_construct(harness: &mut ScenarioHarness, started: &[String]) -> OutstandingOrder {
    let waiting =
        |order: &OutstandingOrder| stage_of(order) == StageId::Construct && !started.contains(&order.workpiece);
    harness.pump_until("the next queued member receives its parked Construct order", |harness| {
        harness.orders().iter().any(waiting)
    });
    let order = harness.orders().into_iter().find(waiting).expect("pump_until saw the parked order");
    harness.pump_until("the parked child reached its real worktree", |harness| {
        harness.ledger().iter().any(|run| run.nonce == order.nonce)
    });
    order
}

#[test]
fn a_construction_still_queued_when_the_product_folds_is_still_admitted() {
    let authority = Repo::with_formatted_example_project();
    let script = MEMBERS.iter().fold(LaneScript::all_passing(), |script, (workpiece, _, _)| {
        script.then_for(*workpiece, StageId::Construct, LaneMode::NeverExits)
    });
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_policy())
        .script(&script)
        .max_concurrent_lanes(1)
        .max_concurrent_provers(2)
        .start("a-queued-construction-survives-the-product-fold");
    let bloom = seal_three(&mut harness);

    // One lane, so exactly one member authors at a time and the other two sit in
    // `queued_construction` on the context the seal handed them.
    let leading = parked_lane_at(&mut harness, StageId::Construct);
    assert_eq!(constructing_members(&harness), vec![leading.workpiece.clone()], "one lane admits one author");
    assert_eq!(coordination(&harness, bloom).queued_construction.len(), 2, "its two siblings are still queued");

    // The leader finishes and the product absorbs it. The slot it frees goes to
    // the next in the queue, so when the head moves the last member is still
    // queued — and its admission is confirmed well after the fold, which is the
    // exact window the old equality check lost.
    author_and_release(
        &mut harness,
        &leading,
        candidate_path(&leading.workpiece),
        "pub fn queued() -> u8 {\n    1\n}\n",
    );
    let following = next_parked_construct(&mut harness, from_ref(&leading.workpiece));
    harness.pump_until("the product absorbs the member that finished first", |harness| {
        covers(&product_coverage(harness, bloom), &leading.workpiece)
    });

    let state = coordination(&harness, bloom);
    let trailing = state.queued_construction.values().map(|dispatch| dispatch.workpiece.0.clone()).collect::<Vec<_>>();
    assert_eq!(trailing.len(), 1, "one member is still queued across the fold: {trailing:?}");
    let trailing = trailing.into_iter().next().expect("the queued member");
    assert_ne!(state.integration.head.candidate.checkout, state.integration.generation.base.checkout);
    assert_eq!(
        state.queued_construction[&trailing].context.starting_head.candidate.checkout,
        state.integration.generation.base.checkout,
        "the queued dispatch is still pinned to the head it was handed, one fold behind the product",
    );

    // Freeing the last lane slot must admit it rather than retire it.
    author_and_release(
        &mut harness,
        &following,
        candidate_path(&following.workpiece),
        "pub fn queued() -> u8 {\n    2\n}\n",
    );
    let trailing_order = next_parked_construct(&mut harness, &[leading.workpiece.clone(), following.workpiece.clone()]);
    assert_eq!(trailing_order.workpiece, trailing, "the member queued across the fold is the one that starts");
    assert!(
        coordination(&harness, bloom).admitted_construction.contains_key(&trailing),
        "and it holds the admission its queued dispatch asked for",
    );

    // The bloom finishes: three members, three folds, one landed product.
    author_and_release(
        &mut harness,
        &trailing_order,
        candidate_path(&trailing_order.workpiece),
        "pub fn queued() -> u8 {\n    3\n}\n",
    );
    harness.pump_until("the product covers every member", |harness| product_coverage(harness, bloom).len() == 3);
    harness.pump_until("the bloom resolves the product its own folds assembled", |harness| {
        replay_snapshot(&mut harness.commission_store())
            .blooms
            .get(&bloom)
            .is_some_and(|record| record.resolved_head.is_some())
    });
    harness.await_landing(bloom, BloomStatus::Landed);
}
