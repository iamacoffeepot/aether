//! An operator re-runs `verify.base` on a red receipt whose failure does not
//! describe the tree, and the withheld construct is released when the second
//! verdict is green.
//!
//! Pre-fix a red base receipt was permanent for the life of the tree it is
//! keyed to (#5477). The machine's seal-side short-circuit is sticky on
//! purpose; what was missing was a human-authored writer into the same
//! ledger.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BaseReverify, BaseReverifyError, Fact, Outcome, StageId, VerifyFailure, VerifyFailureSet};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, Oracle, ScenarioHarness, digest, failed, passed};

const MEMBER: &str = "wp";

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn base_verify_orders(harness: &ScenarioHarness) -> Vec<OutstandingOrder> {
    harness.orders().into_iter().filter(|order| stage_of(order) == StageId::BaseVerify).collect()
}

#[test]
fn a_red_base_reverifies_on_an_operator_decision() {
    let mut harness = FixtureHarness::start("red-base-reverify");
    let (_bloom, outcome) = harness.try_seal(&[(MEMBER, digest(0x53))]);
    assert!(matches!(outcome, Outcome::Sealed(_)), "got {outcome:?}");

    let first = harness.await_order();
    assert_eq!(stage_of(&first), StageId::BaseVerify);
    harness.upload_admitted(&failed(&first, VerifyFailureSet::one(VerifyFailure::Docs)));

    for _ in 0..10 {
        harness.tick();
        assert!(
            harness.orders().iter().all(|order| stage_of(order) != StageId::Construct),
            "a red base must not dispatch construct",
        );
    }

    let alert = harness.view().base_alert.expect("a red base raises a day-level alert");
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));

    let queued = harness.admit(
        "reverify-base",
        Fact::BaseReverify(BaseReverify {
            base: alert.base,
            reason: "the host killed the fan-out".into(),
            operator: "eve".into(),
        }),
    );
    assert!(
        matches!(queued, Outcome::BaseVerifyQueued { base } if base == alert.base),
        "the operator fact queues the same run the seal door would have: {queued:?}",
    );

    assert!(
        harness.view().base_alert.is_none(),
        "a pending overwrite clears the red alert while the re-run is in flight"
    );
    for _ in 0..10 {
        harness.tick();
        assert!(
            harness.orders().iter().all(|order| stage_of(order) != StageId::Construct),
            "construct stays withheld while the re-run is pending",
        );
    }

    let refused = harness.admit(
        "reverify-base-again",
        Fact::BaseReverify(BaseReverify {
            base: alert.base,
            reason: "still think it was the host".into(),
            operator: "eve".into(),
        }),
    );
    assert!(
        matches!(refused, Outcome::BaseReverifyRejected(BaseReverifyError::NotRed)),
        "a second ask while pending is refused as not-red: {refused:?}",
    );

    harness.pump_until("a fresh BaseVerify order goes out", |harness| !base_verify_orders(harness).is_empty());
    let fresh = base_verify_orders(&harness);
    assert_eq!(fresh.len(), 1, "the refused second ask minted no second order: {fresh:?}");
    assert_ne!(fresh[0].nonce, first.nonce, "the re-run is a new order, not the one already answered");

    harness.upload_admitted(&passed(&fresh[0]));
    harness.pump_until("the withheld construct dispatches", |harness| {
        harness.orders().iter().any(|order| stage_of(order) == StageId::Construct)
    });
    assert!(harness.view().base_alert.is_none(), "a green second verdict releases the day");
}
