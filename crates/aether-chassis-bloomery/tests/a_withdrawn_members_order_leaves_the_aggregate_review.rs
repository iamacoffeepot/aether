//! A member an operator withdraws mid-walk stops being an obligation of the
//! composed tree — end to end, through the real seal, the real withdrawal, the
//! real fold, and the real aggregate-review dispatch.
//!
//! The defect this pins (bloom 0f16e207, 2026-09-15): the review order is
//! composed from the bloom's persisted work-order roster, and a
//! `dispatch_description` row is written at dispatch and never removed. Three
//! withdrawn members' orders were still rendered as `## Task` sections, so the
//! critic was asked to judge a tree for work that was deliberately taken out of
//! it. It failed the composition — "the whole order never entered the fold" —
//! and the coordinator answered the red verdict with a weave repair that would
//! have re-authored withdrawn work.
//!
//! Only a live-member scenario can catch it. Every unit around it passes on a
//! roster that happens to have no withdrawal in it, and the reducer's own
//! tests cannot see what text the executor rendered.

use aether_bloomery::{BloomStatus, StageId, Transformation, WorkpieceId};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, OperatorMove, captured, digest, passed};

/// The two members whose work is in the composed tree, and the one the
/// operator pulls out while the bloom walks.
const LIVE: [&str; 2] = ["issue-live", "issue-peer"];
const WITHDRAWN: &str = "issue-gone";

/// A phrase unique to each member's work order, so an assertion about the
/// composed prompt names what it is looking for rather than a workpiece id
/// that could appear for some unrelated reason.
fn work_order(workpiece: &str) -> String {
    format!("Build the {workpiece} widget and wire its handle.")
}

#[test]
fn a_withdrawn_members_order_leaves_the_aggregate_review() {
    let mut harness = FixtureHarness::start("withdrawn-review-scenario");
    let bloom = harness.seal_members(&[(LIVE[0], digest(0x51)), (LIVE[1], digest(0x52)), (WITHDRAWN, digest(0x53))]);
    for workpiece in [LIVE[0], LIVE[1], WITHDRAWN] {
        harness.record_description(bloom, workpiece, &work_order(workpiece));
    }

    // All three members are walking: three construct lanes are out, the
    // withdrawn one included. Withdrawing before this would take a member out
    // of a bloom that had not started, which is not the case that broke.
    harness.await_orders(3);
    let outcome = harness.apply_operator(
        bloom,
        &OperatorMove::Withdraw {
            at_tick: 0,
            workpiece: WorkpieceId(WITHDRAWN.to_owned()),
            reason: "pulled out of the wave".to_owned(),
            operator: "operator".to_owned(),
            cascade: false,
        },
    );
    assert!(
        format!("{outcome:?}").contains("MembersWithdrawn"),
        "the fixture withdrawal must take the member out of the line: {outcome:?}",
    );

    // The withdrawn member's lane is cancelled, so what is left outstanding is
    // exactly the two live constructs. Drive both to a claim.
    for stage in [StageId::Construct, StageId::Verify] {
        let orders = harness.await_orders(2);
        for order in &orders {
            assert_ne!(order.workpiece, WITHDRAWN, "a withdrawn member holds no order: {:?}", order.workpiece);
            let upload = if stage == StageId::Construct {
                let candidate = harness.seed_capture(bloom, &order.workpiece, digest(0xC1), digest(0xC2));
                captured(order, candidate)
            } else {
                passed(order)
            };
            harness.upload_admitted(&upload);
        }
    }

    // The claim set is complete over the live membership, so the fold goes out
    // and both composite gates are dispatched against it.
    harness.integrate_tick();
    let gates = harness.await_orders(2);
    let review = gates
        .iter()
        .find(|order| from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::AggregateReview))
        .expect("the fold dispatches an aggregate review");
    let transformation =
        from_bytes::<Transformation>(&review.transformation).expect("the stored order's transformation decodes");
    let task = transformation.description.as_deref().expect("the critic is given the membership's work orders");

    for workpiece in LIVE {
        assert!(
            task.contains(&work_order(workpiece)),
            "{workpiece} is in the composed tree, so its order is still the critic's subject: {task}",
        );
    }
    assert!(
        !task.contains(&work_order(WITHDRAWN)) && !task.contains(WITHDRAWN),
        "a withdrawn member's work order must not be rendered as an obligation of the fold: {task}",
    );

    harness.land_the_fold(bloom);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed, "the bloom lands on its live membership");
}
