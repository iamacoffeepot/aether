//! The #5330 shape: a successor that inherits both members' claims, then
//! collides on the fold, must re-enter at Verify on the reconciled tree —
//! not Construct against the inherited scope revision.
//!
//! Pre-fix: an inherited claim has no cursor, so `reduce_attempt_completed`'s
//! assembling predicate dispatched Construct against `member.scope_revision`.
//! The lane displayed the reconciled tree, intake refused `DigestMismatch`,
//! and the order stayed live.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomStatus, Fact, Outcome, StageId, Transformation};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, Oracle, captured, digest, draft, member, passed};

const FIRST: &str = "wp-0";
const SECOND: &str = "wp-1";

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
fn a_supersede_adopted_member_that_collides_reconciles_back_to_verify() {
    let mut harness = FixtureHarness::start("supersede-inherit-fold-conflict");
    let predecessor = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52))]);

    let constructs = harness.await_orders(2);
    let first = harness.seed_capture(predecessor, FIRST, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(named(&constructs, FIRST), first));
    let second = harness.seed_capture(predecessor, SECOND, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(named(&constructs, SECOND), second));

    let verifies = harness.await_orders(2);
    harness.upload_admitted(&passed(named(&verifies, FIRST)));
    harness.upload_admitted(&passed(named(&verifies, SECOND)));

    // Same workpieces and scope revisions so claims inherit, but a distinct
    // approval detail so the successor spec is not the predecessor's content
    // address.
    let mut first_member = member(FIRST, digest(0x51));
    first_member.approval.detail = digest(0xAB);
    first_member.approval.subject = first_member.subject();
    let successor_spec = draft(harness.view().mainline, &[first_member, member(SECOND, digest(0x52))]);
    let successor = successor_spec.id();
    harness.seed_fold_conflict(successor, SECOND, vec!["crates/example-shared/src/lib.rs".into()]);
    match harness.admit("inherit-both", Fact::Supersede { predecessor, successor: successor_spec }) {
        Outcome::Superseded { .. } => {}
        other => panic!("the successor must supersede: {other:?}"),
    }
    assert_eq!(harness.bloom(predecessor).status, BloomStatus::Superseded);

    harness.integrate_tick();
    harness.clear_fold_conflict(successor, SECOND);

    let reconcile = harness.await_order();
    assert_eq!(reconcile.workpiece, SECOND, "the later member absorbs reconciliation");
    assert_eq!(stage_of(&reconcile), StageId::Reconcile);
    let transformation: Transformation =
        from_bytes(&reconcile.transformation).expect("a recorded order carries a Transformation");
    assert_ne!(transformation.checkout, digest(0x51), "Reconcile must not check out the inherited scope revision");

    let reconciled = harness.seed_capture(successor, SECOND, digest(0xC3), digest(0xD3));
    harness.upload_admitted(&captured(&reconcile, reconciled));

    let next = harness.await_order();
    assert_eq!(next.workpiece, SECOND);
    assert_eq!(
        stage_of(&next),
        StageId::Verify,
        "a completed Reconcile on an inherited claim returns to Verify, not Construct"
    );
    harness.upload_admitted(&passed(&next));
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
