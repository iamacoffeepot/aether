//! A red base verify withholds construct for the rest of the day and names
//! the failing gate on the view.
//!
//! Pre-fix: a red base was invisible, or a member dispatched anyway.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Outcome, StageId, VerifyFailure, VerifyFailureSet};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, Oracle, digest, failed};

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

#[test]
fn a_red_base_holds_the_day_and_names_the_gate() {
    let mut harness = FixtureHarness::start("red-base-holds-the-day");
    let (_bloom, outcome) = harness.try_seal(&[("wp", digest(0x51))]);
    assert!(matches!(outcome, Outcome::Sealed(_)), "got {outcome:?}");

    let order = harness.await_order();
    assert_eq!(stage_of(&order), StageId::BaseVerify);
    harness.upload_admitted(&failed(&order, VerifyFailureSet::one(VerifyFailure::Docs)));

    for _ in 0..10 {
        harness.tick();
        assert!(
            harness.orders().iter().all(|order| stage_of(order) != StageId::Construct),
            "a red base must not dispatch construct",
        );
    }

    let alert = harness.view().base_alert.expect("a red base raises a day-level alert");
    assert!(
        alert.failed.iter().any(|name| name.contains("docs")),
        "the alert must name the failing gate, got {:?}",
        alert.failed,
    );

    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
