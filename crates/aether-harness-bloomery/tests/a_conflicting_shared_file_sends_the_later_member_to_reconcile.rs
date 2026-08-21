//! Two members write the same file and this time the hunks really do collide:
//! both lanes still run to completion, and the later-canonical member takes an
//! ADR-0189 reconcile lap on the advanced base, carrying the candidate its lane
//! produced.
//!
//! The other half of #5401. Dropping ADR-0204's eviction must not drop the
//! handling of a genuine textual conflict — it moves it to where the trees
//! actually meet. The claim to pin is that the conflict costs one reconcile lap
//! *after* the work exists, seeded from the member's own candidate, rather than
//! a cancel before it exists: the lane was never stopped, so the executor's
//! reuse pool still holds that member's session and the lap resumes it instead
//! of paying for a cold one.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomId, Fact, Outcome, StageId, Transformation, WorkpieceId};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed};

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
fn a_conflicting_shared_file_sends_the_later_member_to_reconcile() {
    let mut harness = FixtureHarness::start("shared-file-conflicts");
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

    // The hunks overlap after all, so the later member's candidate does not
    // merge onto the tree the earlier one folded.
    harness.seed_fold_conflict(bloom, SECOND, vec![SHARED.to_owned()]);
    harness.integrate_tick();
    harness.clear_fold_conflict(bloom, SECOND);

    let reconcile = harness.await_order();
    assert_eq!(reconcile.workpiece, SECOND, "the later-canonical member absorbs reconciliation");
    assert_eq!(stage_of(&reconcile), StageId::Reconcile);

    let transformation: Transformation =
        from_bytes(&reconcile.transformation).expect("a recorded order carries a Transformation");
    assert_eq!(
        transformation.inputs[0],
        digest(0xC2),
        "the lap is seeded from the candidate the member's own lane produced, not from a discarded start",
    );
    assert_ne!(
        transformation.checkout,
        digest(0xD2),
        "the lap checks out the advanced fold head, not the member's own capture",
    );

    let view = harness.bloom(bloom);
    let later = view.members.iter().find(|member| member.workpiece.0 == SECOND).expect("the member is listed");
    assert!(later.evicted_by.is_none(), "the conflict is a reconcile lap, never an eviction: {later:?}");
    assert_eq!(later.machinery_rolls, 0, "a textual conflict is not a machinery fault: {later:?}");
}
