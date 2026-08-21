//! A Construct completion bound to the wrong subject must not leave an
//! order that outlives the scenario.
//!
//! Pre-fix: the refusal left the order live and nothing re-dispatched it,
//! so the coordinator waited forever for an honest upload from a lane that
//! had already exited. The doctor's `pending` escape stayed green because
//! the workpiece was still in `outstanding_orders`; only liveness objected.

#![allow(clippy::unwrap_used)]

use aether_bloomery::StageId;
use aether_bloomery::testing::digest;
use aether_harness_bloomery::{BloomeryHarness, LaneScript, Quiescence, classify};

#[test]
fn a_wrong_subject_does_not_outlive_the_tick_budget() {
    let mut harness = BloomeryHarness::start();
    harness.script_lane(&aether_bloomery::WorkpieceId("wp".into()), StageId::Construct, &[LaneScript::WrongSubject]);
    let bloom = harness.seal_member("wp", digest(0x51));
    harness.run_until(
        |harness| {
            let member = &harness.bloom(bloom).members[0];
            member.wedge.is_some() || member.park.is_some() || member.resolution.is_some()
        },
        40,
    );

    match classify(&harness.view(), &harness.outstanding()) {
        Quiescence::Stalled(why) => panic!("wrong-subject must not stall: {why}"),
        Quiescence::Wedged(_) | Quiescence::Terminal(_) => {}
    }
}
