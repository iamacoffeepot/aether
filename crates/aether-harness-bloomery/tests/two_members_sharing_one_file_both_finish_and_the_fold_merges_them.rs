//! Two members write the same file in disjoint hunks: both lanes run to
//! completion, the fold merges them, and nobody takes a reconcile lap.
//!
//! The bug this pins (#5401): ADR-0204's per-file lease was exclusive and
//! acquired at first observed *write*, so the earlier-canonical member's
//! observation cancelled the later member's live lane the moment the two
//! touched one path — before anything knew whether the edits shared a hunk. On
//! bloom `4360e7e4a081` they did not: the hunks were hundreds of lines apart, a
//! three-way merge applied cleanly, and the cancel threw away five minutes of
//! finished work and charged a machinery roll for a conflict the fold never
//! saw. Nothing in the reducer's own tests crosses the dispatch → executor →
//! intake seam where the cancel actually lands.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomId, BloomStatus, Fact, Outcome, StageId, WorkpieceId};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, Oracle, captured, digest, passed};

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

/// Admit the observation the executor's working-tree sweep would have admitted
/// for one lane.
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
fn two_members_sharing_one_file_both_finish_and_the_fold_merges_them() {
    let mut harness = FixtureHarness::start("shared-file-merges");
    let bloom = harness.seal_members(&[(FIRST, digest(0x51)), (SECOND, digest(0x52))]);

    let constructs = harness.await_orders(2);
    assert!(constructs.iter().all(|order| stage_of(order) == StageId::Construct));

    // The later-canonical member writes the shared file first, then the earlier
    // one writes it too — the exact order that used to evict.
    observe(&mut harness, "writes-second", bloom, SECOND);
    observe(&mut harness, "writes-first", bloom, FIRST);
    for _ in 0..3 {
        harness.dispatch_tick();
    }

    let live = harness.orders();
    assert_eq!(
        live.iter().map(|order| order.nonce.clone()).collect::<Vec<_>>(),
        constructs.iter().map(|order| order.nonce.clone()).collect::<Vec<_>>(),
        "both construct lanes are still live: a shared path is a merge, not a cancel",
    );
    let view = harness.bloom(bloom);
    for member in &view.members {
        assert!(member.evicted_by.is_none(), "no member is evicted off a shared file: {member:?}");
        assert_eq!(member.machinery_rolls, 0, "a shared file costs no machinery roll: {member:?}");
    }

    // Both lanes finish the work they were dispatched for.
    let first = harness.seed_capture(bloom, FIRST, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(named(&constructs, FIRST), first));
    let second = harness.seed_capture(bloom, SECOND, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(named(&constructs, SECOND), second));

    let verifies = harness.await_orders(2);
    assert!(
        verifies.iter().all(|order| stage_of(order) == StageId::Verify),
        "each member goes straight to Verify, with no reconcile lap between: {:?}",
        verifies.iter().map(stage_of).collect::<Vec<_>>(),
    );
    harness.upload_admitted(&passed(named(&verifies, FIRST)));
    harness.upload_admitted(&passed(named(&verifies, SECOND)));

    harness.land_the_fold(bloom);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed, "the merged fold lands");

    let landed = harness.bloom(bloom);
    for member in &landed.members {
        assert!(member.resolution.is_some(), "both members carry their own resolution: {member:?}");
        assert_eq!(member.machinery_rolls, 0, "nothing was charged for the shared file: {member:?}");
    }
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
