//! A member whose `Review` finds against its candidate leaves the bloom under
//! the sealed `Eject` disposition, carrying the judge's findings in its
//! departure reason and leaving its candidate behind (ADR-0221, ADR-0218
//! §Amendment: low tolerance).
//!
//! The disposition is the bloom's, not the gate's. ADR-0218 already decided
//! what a member that did not pass is worth — it leaves, and the candidate
//! stays on its ref for a person to pick up — and a judge's verdict is the same
//! answer to the same question one gate later. A second, gate-specific policy
//! would let one bloom eject on a compiler and bargain with a judge.
//!
//! Pre-fix there is no member `Review` to fail: the scenario cannot reach its
//! subject at all, because a green `Verify` resolves the member and no third
//! order is ever dispatched.
//!
//! What this pins beyond the withdrawal itself is the reason. An ejected
//! member's reader has one question — what stopped it — and the answer has to
//! survive being read out of the GitHub mirror with no journal at hand, so the
//! judge's own words have to be *in* the sentence rather than behind an
//! evidence digest.

#![allow(clippy::unwrap_used)]

use aether_bloomery::StageId;
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, found, passed};

const MEMBER: &str = "wp";
const FINDINGS: &str = "the commission asked for a bounded retry; this candidate loops without a ceiling.";

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

#[test]
fn a_judge_that_finds_against_a_candidate_ejects_its_member() {
    // No sealed policy, so the standing disposition answers: eject.
    let mut harness = FixtureHarness::start("member-review-ejects");
    let bloom = harness.seal_member(MEMBER, digest(0x51));

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    assert_eq!(stage_of(&verify), StageId::Verify);
    harness.upload_admitted(&passed(&verify));

    let review = harness.await_order();
    assert_eq!(stage_of(&review), StageId::Review, "the mechanically green candidate reaches the judge");
    harness.upload_admitted(&found(&review, FINDINGS));

    let member = harness.bloom(bloom).members[0].clone();
    let withdrawn = member.withdrawn.as_ref().expect("a judge that found against the candidate ejects its member");
    assert_eq!(withdrawn.cause, "verify", "one cause for every gate a member did not pass");
    assert!(withdrawn.reason.contains(FINDINGS), "the judge's own words travel with the member: {}", withdrawn.reason);
    assert!(withdrawn.reason.contains("review"), "and the first clause says which gate refused: {}", withdrawn.reason);
    assert!(member.resolution.is_none(), "an ejected member does not fold");

    // No repair roll is spent and no lap is dispatched: the ejection is the
    // whole answer, which is what separates it from the `Refine` disposition.
    assert_eq!(member.machinery_rolls, 0, "an ejection charges no machinery roll: {member:?}");
    harness.pump_until("the ejection dispatches nothing further", |harness| harness.orders().is_empty());

    // And the sentence says where the work went. A withdrawal retires the
    // member's cursor, so the record itself no longer names the tree — which is
    // exactly why the reason has to, and why it is the thing pinned here: it is
    // what a person reads out of the GitHub mirror with no journal at hand.
    assert!(
        withdrawn.reason.contains("left on its ref"),
        "the departure says the candidate was kept, not discarded: {}",
        withdrawn.reason
    );
}
