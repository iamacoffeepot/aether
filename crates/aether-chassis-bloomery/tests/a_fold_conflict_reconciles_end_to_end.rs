//! Two members overlap on the fold: conflict → journaled fact → reconcile
//! dispatch → intake admits the candidate → re-fold → resolve. The stub
//! runner is the scripted-verdict seam; no operator action sits between.
//!
//! This is the path the reducer-only and integrate-only fixtures do not
//! cross — the dispatch → executor → intake seam, where a completed
//! Reconcile used to be refused as out-of-line and the bloom stalled.
//!
//! It is also where the *cost* of that round trip is observable end to end
//! (ADR-0218 §Amendment: reconcile is scoped to the merge): the order the lap
//! runs under and the diff base the confirming Verify is given are both
//! produced by real reducer + reactor code here, so the two halves of the
//! amendment are asserted against the same run rather than against fixtures
//! that could agree with each other and disagree with the coordinator.

use aether_bloomery::{BloomStatus, StageId, Transformation};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed};

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

    // The lap is scoped to the merge, not to re-authoring the member. The paths
    // sit under the heading the executor drain parses, and the order says in so
    // many words that the member's own change is already proved and stays put —
    // the two things whose absence bought bloom 0f16e207 a 15-minute
    // re-construction before the merge was even attempted.
    let (_, listed) = description.split_once("## Conflicting paths\n").expect("the parseable paths heading");
    assert!(listed.contains("- crates/overlap.rs\n"), "the path is a parseable list item: {description}");
    assert!(description.contains("Resolve the merge, and only the merge."), "the lap is merge-only: {description}");
    assert!(
        description.contains("already verified as it stands"),
        "the order says the member's change is already proved: {description}",
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

    // The confirming Verify diffs against the candidate this bloom already
    // proved — `0xD2`, the checkout the member's own Verify passed on — so the
    // mechanical lane's closure covers the merge rather than the member's whole
    // change plus the merge. `sealed_on` is the range it used to be handed.
    let confirming = transformation_of(&verify);
    assert_eq!(
        confirming.diff_base,
        Some(digest(0xD2)),
        "the delta-confirm starts at the proved candidate, not at the bloom base {sealed_on:?}",
    );
    assert_ne!(confirming.diff_base, Some(sealed_on), "the whole change is not re-proved to learn what the merge did");
    assert_eq!(confirming.inputs.first(), Some(&digest(0xC3)), "the subject is still the reconciled candidate");
    assert_eq!(
        confirming.inputs.len(),
        2,
        "the receipt that proved the range's left end rides beside the subject: {:?}",
        confirming.inputs,
    );

    harness.upload_admitted(&passed(&verify));

    harness.land_the_fold(bloom);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed);
}
