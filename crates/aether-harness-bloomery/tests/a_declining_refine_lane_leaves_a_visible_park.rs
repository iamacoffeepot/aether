//! A construct lane that declines to produce a candidate leaves a visible
//! park, not a stall and not a burned retry budget.
//!
//! Pre-fix: no host route minted `EvidenceKind::ConstructDeclined`, so the
//! decline landed in `retry_or_wedge` and burned Construct budget on a
//! refusal a retry reproduces; and even once the route existed, the park
//! was invisible to both oracles.

#![allow(clippy::unwrap_used)]

use aether_bloomery::StageId;
use aether_bloomery::testing::digest;
use aether_harness_bloomery::{BloomeryHarness, LaneScript, Oracle};

#[test]
fn a_declining_construct_lane_parks_the_member() {
    let mut harness = BloomeryHarness::start();
    harness.script_lane(&aether_bloomery::WorkpieceId("wp".into()), StageId::Construct, &[LaneScript::Decline]);
    let bloom = harness.seal_member("wp", digest(0x51));
    harness.run_until(|harness| harness.bloom(bloom).members.iter().any(|member| member.park.is_some()), 40);

    let view = harness.bloom(bloom);
    let member = &view.members[0];
    assert!(member.park.is_some(), "the decline is a named park: {member:?}");
    assert!(member.wedge.is_none(), "a park is not a wedge");
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
