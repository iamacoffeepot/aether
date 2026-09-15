//! The other direction of the empty-verdict rule: a clean review that names
//! what it read passes the same gate, and its fold lands.
//!
//! A clean review stamps no findings — that is what "clean" means — so the rule
//! that refuses an empty verdict is one field away from refusing every passing
//! review there is. If it did, every bloom in the fleet would sit on an
//! aggregate-review fault series until its budget ran out, and the coordinator
//! would land nothing at all. That failure is total and silent from inside the
//! refusing scenario, which is why it is asserted here rather than left to be
//! noticed.
//!
//! The note is the whole difference between the two scenarios: the same member
//! line, the same fold, the same gates, and one string saying what the critic
//! read.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomStatus, StageId};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed, reviewed};

const MEMBER: &str = "wp-0";

#[test]
fn a_review_that_says_what_it_read_lands_the_fold() {
    let mut harness = FixtureHarness::start("review-says-what-it-read");
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

    harness.integrate_tick();
    let gates = harness.await_orders(2);
    for order in &gates {
        let is_review = from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::AggregateReview);
        let upload = if is_review {
            reviewed(order)
        } else {
            passed(order)
        };
        let key = harness.upload_admitted(&upload);
        if is_review {
            assert!(
                key.starts_with("aether.bloomery.aggregate_review:"),
                "a review that says what it read is a completion, not a fault: {key}",
            );
        }
    }

    harness.land_tick();
    assert!(
        harness.bloom(bloom).executor_fault.is_none(),
        "a review that reported no defect and said what it read is a pass, not a host fault",
    );
    harness.await_landing(bloom, BloomStatus::Landed);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed, "the reviewed fold lands");
}
