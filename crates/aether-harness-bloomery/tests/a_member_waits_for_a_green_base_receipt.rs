//! A seal onto an unproven base withholds construct until `verify.base` is
//! green.
//!
//! Pre-fix: `reduce_seal` dispatched every ready member's entry
//! unconditionally, so `harness.orders()` after the seal contained a
//! `construct.implement` order and no `verify.base` order at all — the
//! `8e9616337714` loop.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Outcome, StageId, Transformation, VERIFY_BASE_COMMAND};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, digest, passed};

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn command_of(order: &OutstandingOrder) -> String {
    from_bytes::<Transformation>(&order.transformation).expect("a recorded order carries a Transformation").command
}

#[test]
fn a_member_waits_for_a_green_base_receipt() {
    let mut harness = FixtureHarness::start("member-waits-green-base");
    let (bloom, outcome) = harness.try_seal(&[("wp", digest(0x51))]);
    assert!(matches!(outcome, Outcome::Sealed(_)), "got {outcome:?}");

    let order = harness.await_order();
    assert_eq!(stage_of(&order), StageId::BaseVerify);
    assert_eq!(command_of(&order), VERIFY_BASE_COMMAND);
    assert!(
        harness.orders().iter().all(|order| stage_of(order) != StageId::Construct),
        "a seal must not dispatch construct onto an unproven base",
    );

    let before = harness.bloom(bloom).members[0].cursor.clone();
    harness.upload_admitted(&passed(&order));

    let construct = harness.await_order();
    assert_eq!(stage_of(&construct), StageId::Construct);
    let after = harness.bloom(bloom).members[0].cursor.clone();
    if let (Some(before), Some(after)) = (before.as_ref(), after.as_ref()) {
        assert!(after.attempts >= before.attempts, "the member's cursor must not move backwards");
    }
}
