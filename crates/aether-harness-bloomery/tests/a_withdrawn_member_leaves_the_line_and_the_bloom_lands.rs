//! An operator withdraws one member of a two-member bloom; the bloom folds and
//! lands on the survivor alone.
//!
//! The claim to pin is that a withdrawal is not a stall. Three folds in the
//! reducer are otherwise total over the sealed member list — the claim-set
//! completeness check, the fold's candidate list, and the resolve's per-member
//! claim scan — so a member that will never produce a claim pins the bloom, its
//! sibling's finished work, and the mainline behind it (#5327). Nothing in the
//! reducer's own tests crosses the dispatch → executor → intake seam this does.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomStatus, Outcome, WorkpieceId};
use aether_harness_bloomery::{FixtureHarness, OperatorMove, captured, digest, passed};

const SURVIVOR: &str = "wp-0";
const LEAVING: &str = "wp-1";

#[test]
fn a_withdrawn_member_leaves_the_line_and_the_bloom_lands() {
    let mut harness = FixtureHarness::start("withdraw-one-member");
    let bloom = harness.seal_members(&[(SURVIVOR, digest(0x51)), (LEAVING, digest(0x52))]);

    // Both members are dispatched; only the survivor ever answers.
    let constructs = harness.await_orders(2);
    let survivor_order = constructs
        .iter()
        .find(|order| order.workpiece == SURVIVOR)
        .unwrap_or_else(|| panic!("no construct order for {SURVIVOR}"));
    let candidate = harness.seed_capture(bloom, SURVIVOR, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(survivor_order, candidate));

    let outcome = harness.apply_operator(
        bloom,
        &OperatorMove::Withdraw {
            at_tick: 0,
            workpiece: WorkpieceId(LEAVING.into()),
            reason: "the work moved to a later bloom".into(),
            operator: "harness".into(),
            cascade: false,
        },
    );
    assert!(
        matches!(&outcome, Outcome::MembersWithdrawn { withdrawn, terminal: false, .. } if withdrawn.len() == 1),
        "one named member leaves and the bloom keeps walking: {outcome:?}",
    );

    let view = harness.bloom(bloom);
    let leaving = view.members.iter().find(|member| member.workpiece.0 == LEAVING).expect("the member is still listed");
    assert!(leaving.withdrawn.is_some(), "a withdrawn member is visible as withdrawn, not simply absent: {leaving:?}");
    assert!(leaving.wedge.is_none(), "a withdrawal is not a wedge");

    // The survivor finishes its line. The withdrawn member never dispatched
    // again, so this is the only order outstanding.
    let verify = harness.await_order();
    assert_eq!(verify.workpiece, SURVIVOR, "the withdrawn member dispatches nothing further");
    harness.upload_admitted(&passed(&verify));

    harness.land_the_fold(bloom);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed, "the fold of the survivors lands");
}
