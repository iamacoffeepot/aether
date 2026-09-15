//! A fold that does not build parks the bloom under an operator hold, once,
//! and nothing else goes out (ADR-0218 §Amendment: low tolerance, 2026-09-15).
//!
//! A red aggregate verify implicates no member — that is what makes it the
//! whole-bloom half of the amendment, and what makes it the arm most likely to
//! loop. The old path re-wove the composition and dispatched both gates again,
//! so a fold that does not build spent a paid critic lane per round until the
//! catalog budget ran out. The failure this closes the door on is not the first
//! re-weave; it is the coordinator quietly going round again while nobody is
//! looking at the fold.
//!
//! Driven through the whole loop rather than the reducer because the *quiet*
//! half is what matters: the reducer deciding to park is one assertion, and the
//! integrate reactor declining to dispatch the next round is the other, and
//! only the loop can make the second one.

use aether_bloomery::{BloomStatus, StageId};
use aether_chassis_bloomery::bloomery::{ScriptedUpload, ScriptedVerdict};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed, reviewed, verdict};

/// The bloom's single member. One is enough: the fold is what fails, and it
/// fails over whatever the members produced.
const WORKPIECE: &str = "wp";

#[test]
fn a_red_aggregate_verify_parks_the_bloom_once() {
    let mut harness = FixtureHarness::start("red-aggregate-parks-scenario");
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));

    // The claim set is complete, so the fold goes out with both composite gates
    // over it. The critic passes; the compiler does not.
    harness.integrate_tick();
    let gates = harness.await_orders(2);
    for order in &gates {
        let is_review = from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::AggregateReview);
        let upload = if is_review {
            reviewed(order)
        } else {
            ScriptedUpload {
                findings: Some(String::from("the fold does not build: E0433 in wp/src/lib.rs")),
                ..verdict(order, ScriptedVerdict::VerificationFailed)
            }
        };
        harness.upload_admitted(&upload);
    }

    let parked = harness.bloom(bloom);
    let hold = parked.operator_hold.as_ref().expect("the red fold raises an operator hold");
    assert!(hold.reason.contains("aggregate verify"), "the hold names the gate that refused: {}", hold.reason);
    assert!(parked.review_park.is_some(), "the fold is held as the owner's decision context");
    assert_ne!(parked.status, BloomStatus::Landed, "a bloom whose fold does not build cannot land");

    // The load-bearing negative. The integrate and land reactors are given
    // every chance to go round again; a re-woven composition or a second pair
    // of gate orders here is the loop the amendment closes.
    for _ in 0..3 {
        harness.integrate_tick();
        harness.land_tick();
        harness.dispatch_tick();
    }
    assert!(harness.orders().is_empty(), "a parked fold dispatches nothing further: {:?}", harness.orders());
    assert!(harness.landing_proposal(bloom).is_none(), "and proposes no landing");
}
