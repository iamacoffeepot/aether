//! A bloom that sealed the repair disposition answers a judge's finding with
//! one `Refine` lap steered by that finding, and the lap's output is judged
//! again (ADR-0221).
//!
//! The counterpart to the ejection scenario: same verdict, same gate, opposite
//! sealed answer. What this pins is that the repair arm is a *lap* and not a
//! re-roll — the member returns through `Verify` before it reaches the judge
//! again, because the lap changed the code and the compiler's answer about the
//! old tree does not carry to the new one.
//!
//! Pre-fix the scenario cannot reach its subject: no member `Review` is ever
//! dispatched, so there is no finding for the lap to be steered by.
//!
//! The findings row is the load-bearing half. A repair lap that is not handed
//! what the judge said is a fresh construct against the same commission, which
//! reproduces the same candidate; the row the intake writes at the member's own
//! `Review` is what the lane's `## Findings` section renders from, and it is
//! the same row the aggregate path's repair lap already reads.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{StageId, Transformation};
use aether_chassis_bloomery::store::{OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, found, passed, reviewed};

const MEMBER: &str = "wp";
const FINDINGS: &str = "the commission asked for a bounded retry; this candidate loops without a ceiling.";

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

#[test]
fn a_judged_member_on_the_repair_disposition_takes_one_lap_and_is_rejudged() {
    let mut harness = FixtureHarness::start_refining("member-review-repairs");
    let bloom = harness.seal_member(MEMBER, digest(0x51));

    let construct = harness.await_order();
    let first = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, first));

    let verify = harness.await_order();
    assert_eq!(stage_of(&verify), StageId::Verify);
    harness.upload_admitted(&passed(&verify));

    let review = harness.await_order();
    assert_eq!(stage_of(&review), StageId::Review);
    harness.upload_admitted(&found(&review, FINDINGS));

    // The member stays in the line under this disposition, carrying one roll.
    let refine = harness.await_order();
    assert_eq!(stage_of(&refine), StageId::Refine, "a sealed repair disposition buys the lap the judge asked for");
    assert_eq!(refine.workpiece, MEMBER);
    let judged = harness.bloom(bloom).members[0].clone();
    assert!(judged.withdrawn.is_none(), "the member did not leave: {judged:?}");
    assert_eq!(
        judged.cursor.as_ref().map(|cursor| cursor.stage),
        Some(StageId::Refine),
        "and its cursor sits at the repair lane rather than at the terminus it failed: {judged:?}",
    );

    // And the lap is handed what the judge wrote, on the same row the aggregate
    // path's repair lap reads.
    let steer = harness
        .commission_store()
        .lookup_review_findings(bloom.0.as_bytes(), MEMBER)
        .expect("the findings row reads back")
        .expect("a red judge files what it found against the member it judged");
    assert!(steer.contains(FINDINGS), "the lap is steered by the judge's own words, got {steer}");

    // And the row reaches the lane, not just the store: the dispatched work
    // order carries the finding in its own `## Findings` section, which is what
    // a construct lane reads. Without this the lap is a fresh construct against
    // the same commission and reproduces the candidate the judge refused.
    let order: Transformation =
        from_bytes(&refine.transformation).expect("a recorded order carries its transformation");
    let task = order.description.unwrap_or_default();
    assert!(task.contains(FINDINGS), "the repair lane is handed the finding it is repairing, got {task}");

    // The lap produces a new tree, which goes back through the compiler before
    // it reaches the judge again.
    let second = harness.seed_capture(bloom, MEMBER, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(&refine, second));

    let reverify = harness.await_order();
    assert_eq!(stage_of(&reverify), StageId::Verify, "a repaired tree is compiled again before it is judged again");
    harness.upload_admitted(&passed(&reverify));

    let rejudge = harness.await_order();
    assert_eq!(stage_of(&rejudge), StageId::Review, "and the line returns to its terminus");
    assert_eq!(
        aether_bloomery::Digest::from_slice(&rejudge.displayed_digest),
        Some(second.tree),
        "the second judgement reads the repaired tree, not the one it already found against",
    );

    harness.upload_admitted(&reviewed(&rejudge));
    assert!(
        harness.bloom(bloom).members[0].resolution.is_some(),
        "the passing second judgement is what carries the member into the fold",
    );
}
