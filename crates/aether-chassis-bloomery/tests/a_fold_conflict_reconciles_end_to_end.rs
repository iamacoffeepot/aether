//! Two members overlap on the fold: conflict → journaled fact → reconcile
//! dispatch → intake admits the candidate → re-fold → resolve. The stub
//! runner is the scripted-verdict seam; no operator action sits between.
//!
//! This is the path the reducer-only and integrate-only fixtures do not
//! cross — the dispatch → executor → intake seam, where a completed
//! Reconcile used to be refused as out-of-line and the bloom stalled.

mod common;
pub mod fixture;

use aether_bloomery::{BloomStatus, StageId, Transformation};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use fixture::{FixtureHarness, captured, digest, passed};

const FIRST: &str = "wp-0";
const SECOND: &str = "wp-1";

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn transformation_of(order: &OutstandingOrder) -> Transformation {
    from_bytes(&order.transformation).expect("a recorded order carries a Transformation")
}

fn named<'a>(orders: &'a [OutstandingOrder], workpiece: &str) -> &'a OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece == workpiece)
        .unwrap_or_else(|| panic!("no outstanding order for {workpiece}"))
}

#[test]
fn a_two_member_overlap_reconciles_and_lands() {
    let mut harness = FixtureHarness::start("fold-conflict-reconcile");
    let sealed_on = harness.view().mainline;
    let bloom = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52))]);
    harness.record_description(bloom, SECOND, "add the overlapping widget");

    let constructs = harness.await_orders(2);
    let first = harness.seed_capture(bloom, FIRST, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(named(&constructs, FIRST), first));
    let second = harness.seed_capture(bloom, SECOND, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(named(&constructs, SECOND), second));

    let verifies = harness.await_orders(2);
    harness.upload_admitted(&passed(named(&verifies, FIRST)));
    harness.upload_admitted(&passed(named(&verifies, SECOND)));

    harness.seed_fold_conflict(bloom, SECOND, vec!["crates/overlap.rs".into()]);
    harness.integrate_tick();
    harness.clear_fold_conflict(bloom, SECOND);

    let reconcile = harness.await_order();
    assert_eq!(reconcile.workpiece, SECOND, "the later member absorbs reconciliation");
    assert_eq!(stage_of(&reconcile), StageId::Reconcile);
    let transformation = transformation_of(&reconcile);
    assert_ne!(transformation.checkout, sealed_on, "Reconcile checks out the folded checkpoint, not the sealed base");
    let description = transformation.description.as_deref().unwrap_or("");
    assert!(description.contains("add the overlapping widget"), "the original description is still the task");
    assert!(description.contains("## Fold conflict"), "the contract is assembled in-channel");
    assert!(description.contains("crates/overlap.rs"), "the conflicting path is named");
    assert!(
        description.contains("## Conflicted candidate") && description.contains("diff --git"),
        "the member's conflicted work is in the work order: {description}",
    );

    let reconciled = harness.seed_capture(bloom, SECOND, digest(0xC3), digest(0xD3));
    let key = harness.upload_admitted(&captured(&reconcile, reconciled));
    assert!(
        key.starts_with("aether.bloomery.attempt:"),
        "a completed Reconcile admits AttemptCompleted, not an out-of-line refusal: {key}",
    );

    let verify = harness.await_order();
    assert_eq!(verify.workpiece, SECOND);
    assert_eq!(stage_of(&verify), StageId::Verify);
    harness.upload_admitted(&passed(&verify));

    harness.land_the_fold(bloom);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed);
}
