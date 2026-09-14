//! A member that reconciles after a fold collision produces a candidate the
//! *next fold* has to merge, and a fold merges the candidate ref. The ref and
//! the journal cursor are written by different halves of the coordinator, so
//! this walks the seam between them.
//!
//! The bug this pins (#5992): `admitted_candidate_pushes` had arms for a
//! passing Construct or Refine and for a failing Construct, and `Reconcile` fell
//! to `_ => continue`. Nothing published the lap's commit. The cursor advanced,
//! Verify checked out the cursor's commit and passed on it, and the member read
//! healthy in every view — then the combining fold merged the pre-Reconcile
//! commit from hours earlier, raised the identical `FoldConflict`, and the
//! reducer wedged the member at Reconcile with its budget spent. On a
//! 24-member bloom it took three members down at once, and no surface between
//! the lap and the wedge said anything was wrong.
//!
//! So the walk is: reconcile, pass Verify, and look at the state the missed
//! push leaves — the doctor's `member_candidate_ref_matches_cursor` row is the
//! surface that was silent. Then publish the capture, which is the one step the
//! executor arm now takes, and let the fold do what it was always supposed to:
//! merge the reconciled commit, clean, and land.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomStatus, StageId};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, Oracle, captured, digest, passed};

const FIRST: &str = "wp-0";
const SECOND: &str = "wp-1";
const SHARED: &str = "crates/example-shared/src/lib.rs";
const REF_MATCHES_CURSOR: &str = "member_candidate_ref_matches_cursor";

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn named<'a>(orders: &'a [OutstandingOrder], workpiece: &str) -> &'a OutstandingOrder {
    orders
        .iter()
        .find(|order| order.workpiece == workpiece)
        .unwrap_or_else(|| panic!("no outstanding order for {workpiece}"))
}

/// One named row from a fresh doctor pass: whether it held, and what it named.
fn doctor_row(harness: &mut FixtureHarness, name: &str) -> (bool, String) {
    harness.doctor_tick();
    let report = harness.doctor().expect("the doctor publishes a pass");
    let check = report.named(name).unwrap_or_else(|| panic!("the seed list includes {name}"));
    (check.passed, check.divergences.join("; "))
}

#[test]
fn a_reconcile_lap_reaches_the_fold_through_the_candidate_ref() {
    let mut harness = FixtureHarness::start("reconcile-reaches-the-fold");
    let bloom = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52))]);

    let constructs = harness.await_orders(2);
    let first = harness.seed_capture(bloom, FIRST, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(named(&constructs, FIRST), first));
    let constructed = harness.seed_capture(bloom, SECOND, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(named(&constructs, SECOND), constructed));

    let verifies = harness.await_orders(2);
    harness.upload_admitted(&passed(named(&verifies, FIRST)));
    harness.upload_admitted(&passed(named(&verifies, SECOND)));

    // The hunks overlap, so the later member's candidate does not merge onto the
    // tree the earlier one folded, and it takes an ADR-0189 lap.
    harness.seed_fold_conflict(bloom, SECOND, vec![SHARED.to_owned()]);
    harness.integrate_tick();
    harness.clear_fold_conflict(bloom, SECOND);

    let reconcile = harness.await_order();
    assert_eq!(reconcile.workpiece, SECOND, "the later-canonical member absorbs reconciliation");
    assert_eq!(stage_of(&reconcile), StageId::Reconcile);

    // The lap commits on the settled checkpoint and passes Verify. Its capture is
    // deliberately left unpublished: that is precisely the state the missing push
    // arm produced, and this cell substitutes the push either way, so leaving it
    // out is the only way to stand in it.
    let reconciled = harness.seed_unpublished_capture(SECOND, digest(0xC3), digest(0xD3));
    harness.upload_admitted(&captured(&reconcile, reconciled));
    let reverify = harness.await_order();
    assert_eq!(stage_of(&reverify), StageId::Verify, "a passing fold-time reconcile rejoins at Verify");
    harness.upload_admitted(&passed(&reverify));

    assert_eq!(
        harness.candidate_ref_commit(bloom, SECOND),
        Some(constructed.checkout),
        "the ref still names the pre-lap commit — what the next fold would have merged",
    );
    let view = harness.bloom(bloom);
    let collider = view.members.iter().find(|member| member.workpiece.0 == SECOND).expect("the member is listed");
    assert!(collider.wedge.is_none(), "and the member reads healthy — which is why nothing else catches it");

    let (held, divergences) = doctor_row(&mut harness, REF_MATCHES_CURSOR);
    assert!(!held, "the drift between the ref and the claim is a violation");
    assert!(divergences.contains(SECOND), "the doctor names the drifted member: {divergences}");
    assert!(
        divergences.contains(&digest(0xC3).to_hex()),
        "and the reconciled candidate the member claims: {divergences}",
    );

    // Publishing the capture is the one step the executor's Reconcile arm takes.
    harness.publish_capture(bloom, SECOND, &reconciled);
    assert_eq!(
        harness.candidate_ref_commit(bloom, SECOND),
        Some(reconciled.checkout),
        "the candidate ref now names the reconciled commit",
    );
    let (repaired, still_divergent) = doctor_row(&mut harness, REF_MATCHES_CURSOR);
    assert!(repaired, "the doctor clears once the two agree: {still_divergent}");

    harness.land_the_fold(bloom);
    let landed = harness.bloom(bloom);
    assert_eq!(landed.status, BloomStatus::Landed, "the reconciled candidate folds clean and the bloom lands");
    for member in &landed.members {
        assert!(member.wedge.is_none(), "no member paid for a second collision: {member:?}");
        assert!(member.resolution.is_some(), "both members carry their own resolution: {member:?}");
    }
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
