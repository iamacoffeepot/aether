//! A member whose mechanical `Verify` goes green is judged by one
//! `review.critic` dispatch before it resolves, and the verdict it resolves on
//! is the judge's (ADR-0221).
//!
//! The bug this pins: under ADR-0153 the member line ended at `Verify`, so a
//! passing mechanical gate minted the member's `ResolutionClaim` and the
//! candidate folded with no model having read it. In bloom `0c5a157ecbe4` on
//! 2026-09-15 that produced a bloom with no `Review` dispatch at all — the only
//! judgement anywhere in it was one `AggregateReview` over the already-folded
//! product, downstream of the point where a bad candidate could still be kept
//! out. With the construct seat on the contributor tier, a compiler was the
//! whole quality gate.
//!
//! Pre-fix this fails at the `await_orders` after the verify uploads: no third
//! round of orders is ever dispatched, because both members have resolved.
//!
//! Two members rather than one, because the count is half the assertion. One
//! judge per member — not one per bloom, which is what `AggregateReview`
//! already is, and not one per verify attempt, which would turn a per-member
//! gate into unbounded model spend.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomStatus, Digest, StageId, Transformation};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, Oracle, captured, digest, passed, reviewed};

const FIRST: &str = "wp-0";
const SECOND: &str = "wp-1";

/// The typed command the coordinator dispatched this order under — the lane an
/// executor routes on, read off the order's own sealed transformation rather
/// than inferred from its stage.
fn command_of(order: &OutstandingOrder) -> String {
    from_bytes::<Transformation>(&order.transformation).expect("a recorded order carries its transformation").command
}

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn named<'a>(orders: &'a [OutstandingOrder], workpiece: &str) -> &'a OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece == workpiece)
        .unwrap_or_else(|| panic!("no outstanding order for {workpiece}"))
}

#[test]
fn a_green_member_is_judged_once_before_it_folds() {
    let mut harness = FixtureHarness::start("member-review-before-fold");
    let bloom = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52))]);

    let constructs = harness.await_orders(2);
    assert!(constructs.iter().all(|order| stage_of(order) == StageId::Construct));
    let first = harness.seed_capture(bloom, FIRST, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(named(&constructs, FIRST), first));
    let second = harness.seed_capture(bloom, SECOND, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(named(&constructs, SECOND), second));

    let verifies = harness.await_orders(2);
    assert!(verifies.iter().all(|order| stage_of(order) == StageId::Verify));
    harness.upload_admitted(&passed(named(&verifies, FIRST)));
    harness.upload_admitted(&passed(named(&verifies, SECOND)));

    // The green gate advances rather than resolving: the compiler has spoken
    // and the judge has not.
    for member in &harness.bloom(bloom).members {
        assert!(member.resolution.is_none(), "a green Verify no longer carries a member into the fold: {member:?}");
    }

    let reviews = harness.await_orders(2);
    assert_eq!(reviews.len(), 2, "one judge per member, no more and no fewer");
    for order in &reviews {
        assert_eq!(stage_of(order), StageId::Review, "each green candidate is handed to the judge");
        assert_eq!(command_of(order), "review.critic", "the member's judge runs the critic lane");
    }
    assert_eq!(
        Digest::from_slice(&named(&reviews, FIRST).displayed_digest),
        Some(first.tree),
        "it judges the member's own candidate, not a folded product",
    );
    assert_eq!(Digest::from_slice(&named(&reviews, SECOND).displayed_digest), Some(second.tree));

    harness.upload_admitted(&reviewed(named(&reviews, FIRST)));
    harness.upload_admitted(&reviewed(named(&reviews, SECOND)));

    let judged = harness.bloom(bloom);
    for member in &judged.members {
        assert!(member.resolution.is_some(), "the passing judge is what resolves the member: {member:?}");
    }

    harness.land_the_fold(bloom);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed, "the judged fold lands");

    // Nothing is still owed. A re-dispatched terminus would sit here as a live
    // order, which is also what the liveness oracle below reads.
    assert!(harness.outstanding().is_empty(), "the line owes no further dispatch: {:?}", harness.outstanding());
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
