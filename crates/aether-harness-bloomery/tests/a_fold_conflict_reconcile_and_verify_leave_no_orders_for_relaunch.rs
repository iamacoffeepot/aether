#![allow(clippy::unwrap_used)]

//! Issue 5980: a fold-conflict lap's orders must not relaunch until the bloom lands.
//!
//! A member that goes through Reconcile after a fold conflict, has its candidate
//! admitted, passes Verify, and resolves must leave no `submitted` row behind for
//! either lap: the Reconcile order and the Verify order are consumed on admission,
//! so the executor sees no submitted order with no tracked run to launch again.

use aether_bloomery::{BloomId, Fact, Outcome, StageId, WorkpieceId};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed, reviewed};

const FIRST: &str = "wp-0";
const SECOND: &str = "wp-1";
const SHARED: &str = "crates/example-shared/src/lib.rs";
const OBSERVED_AT_MILLIS: u64 = 1_700_000_000_000;

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn named<'a>(orders: &'a [OutstandingOrder], workpiece: &str) -> &'a OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece == workpiece)
        .unwrap_or_else(|| panic!("no outstanding order for {workpiece}"))
}

fn observe(harness: &mut FixtureHarness, key: &str, bloom: BloomId, workpiece: &str) {
    let fact = Fact::LaneWritesObserved {
        bloom,
        workpiece: WorkpieceId(workpiece.to_owned()),
        stage: StageId::Construct,
        paths: vec![SHARED.to_owned()],
        observed_at: OBSERVED_AT_MILLIS,
    };
    match harness.admit(key, fact) {
        Outcome::LeasesObserved { .. } => {}
        other => panic!("the lane-write observation must be admitted: {other:?}"),
    }
}

#[test]
fn a_fold_conflict_reconcile_and_verify_leave_no_orders_for_relaunch() {
    let mut harness = FixtureHarness::start("fold-conflict-orders-retire");
    let bloom = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52))]);

    let constructs = harness.await_orders(2);
    observe(&mut harness, "writes-second", bloom, SECOND);
    observe(&mut harness, "writes-first", bloom, FIRST);
    for _ in 0..3 {
        harness.dispatch_tick();
    }
    assert_eq!(harness.orders().len(), 2, "neither lane is cancelled on the shared path");

    let first = harness.seed_capture(bloom, FIRST, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(named(&constructs, FIRST), first));
    let second = harness.seed_capture(bloom, SECOND, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(named(&constructs, SECOND), second));

    let verifies = harness.await_orders(2);
    harness.upload_admitted(&passed(named(&verifies, FIRST)));
    harness.upload_admitted(&passed(named(&verifies, SECOND)));

    // The line's terminus: a green Verify advances to the judge, and it is the
    // passing judgement that resolves each member (ADR-0221).
    let reviews = harness.await_orders(2);
    harness.upload_admitted(&reviewed(named(&reviews, FIRST)));
    harness.upload_admitted(&reviewed(named(&reviews, SECOND)));

    harness.seed_fold_conflict(bloom, SECOND, vec![SHARED.to_owned()]);
    harness.integrate_tick();
    harness.clear_fold_conflict(bloom, SECOND);

    let reconcile = harness.await_order();
    assert_eq!(reconcile.workpiece, SECOND, "the later-canonical member absorbs reconciliation");
    assert_eq!(stage_of(&reconcile), StageId::Reconcile);
    let reconcile_nonce = reconcile.nonce.clone();

    let reconciled = harness.seed_capture(bloom, SECOND, digest(0xE1), digest(0xF1));
    harness.upload_admitted(&captured(&reconcile, reconciled));

    let verify = harness.await_order();
    assert_eq!(verify.workpiece, SECOND, "the reconcile lap advances to verify");
    assert_eq!(stage_of(&verify), StageId::Verify);
    let verify_nonce = verify.nonce.clone();
    harness.upload_admitted(&passed(&verify));

    let outstanding = harness.outstanding();
    assert!(
        !outstanding.contains(&reconcile_nonce),
        "the reconcile order is consumed, not left submitted for relaunch",
    );
    assert!(!outstanding.contains(&verify_nonce), "the verify order is consumed, not left submitted for relaunch");

    for _ in 0..3 {
        harness.dispatch_tick();
        let outstanding = harness.outstanding();
        assert!(!outstanding.contains(&reconcile_nonce), "the reconcile nonce is not relaunched");
        assert!(!outstanding.contains(&verify_nonce), "the verify nonce is not relaunched");
    }
    assert!(harness.orders().is_empty(), "no submitted rows remain once both laps resolve");
}
