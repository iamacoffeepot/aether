//! A review completion carrying neither a finding nor a note never judged the
//! fold, and must not land it.
//!
//! This is the shape a critic leaves when its report server was never
//! reachable. The Claude harness injects the `review` MCP server, the critic's
//! `report_finding` / `report_note` tools live behind it, and the lane derives
//! its verdict from the file those tools write. A server the CLI dropped at
//! launch leaves that file empty — and an empty findings file is also exactly
//! what a clean review leaves, so the lane stamped `pass` and intake admitted
//! it. Every aggregate review from 2026-08-17 on took that path: the transcript
//! recorded `"mcp_servers": [{"name": "review", "status": "failed"}]`, both
//! report files were zero bytes, and the fold landed as reviewed.
//!
//! The note channel is what separates the two, and this scenario is the half no
//! unit test over the lane can reach: the verdict has to cross the executor's
//! evidence reader and the admission door before anything can refuse it. The
//! fault is the right answer rather than a refusal — a refusal leaves the order
//! live with nothing to answer it, while a fault is retryable against the
//! review's own budget and charges no member for a host it did not break.
//!
//! The opposite direction is its sibling scenario,
//! `a_review_that_says_what_it_read_lands_the_fold`: a rule that refused every
//! review would wedge every bloom in this suite.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomStatus, StageId};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed, reviewed};

const MEMBER: &str = "wp-0";

#[test]
fn a_review_that_reports_nothing_is_refused_at_intake() {
    let mut harness = FixtureHarness::start("review-reports-nothing");
    let bloom = harness.seal_members(&[(MEMBER, digest(0x51))]);

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));

    // The line's terminus: a green Verify advances to the judge, and it is the
    // passing judgement that resolves the member (ADR-0221).
    let member_review = harness.await_order();
    harness.upload_admitted(&reviewed(&member_review));

    // Both bloom-level gates go out over the same fold. The compiler's pass is
    // a real verdict; the critic's is the empty one — `passed` carries no
    // finding and no note, which is byte-for-byte what the unreachable server
    // produced on the host.
    harness.integrate_tick();
    let gates = harness.await_orders(2);
    for order in &gates {
        assert!(order.workpiece.is_empty(), "a bloom-level order carries no member axis");
        let key = harness.upload_admitted(&passed(order));
        if from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::AggregateReview) {
            assert!(
                key.starts_with("aether.bloomery.aggregate_review_executor_fault:"),
                "an empty review verdict admits as a fault, never as a completion: {key}",
            );
        }
    }

    harness.land_tick();
    let view = harness.bloom(bloom);
    let fault = view.executor_fault.expect("the empty verdict raises the review's executor-fault series");
    assert_eq!(fault.rolls, 1, "one empty verdict is one roll of the review's own budget: {fault:?}");
    assert_ne!(view.status, BloomStatus::Landed, "a fold nobody judged does not land: {:?}", view.status);
    assert!(harness.landing_proposal(bloom).is_none(), "and no landing is proposed for it either");

    // The whole reason this is a fault and not a failing review: nobody wrote a
    // defective candidate, so nobody owes a repair lap for it.
    let member = &view.members[0];
    assert!(member.wedge.is_none(), "the member is charged nothing for a lane that never judged it: {member:?}");
    assert!(member.resolution.is_some(), "and its claim survives the outage: {member:?}");
}
