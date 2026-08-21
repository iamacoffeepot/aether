//! A construct-family lane that cannot finish inside its declared surface
//! names the paths it needs, and the member parks awaiting a person rather
//! than vanishing (ADR-0207).
//!
//! The sibling of `a_declining_refine_lane_leaves_a_visible_park`: that one
//! pins the plain, request-less park; this one pins the typed request. The
//! whole lane → executor → intake → reducer → projection → `/view` path is
//! real, which is the only place the request can be observed at all — the
//! paths cross four crates between the evidence file and the served document.
//!
//! Pre-fix: nothing read the lane's request. A decline reached `/view` as a
//! bare park with no remedy attached, so an operator saw a stopped member and
//! no statement of what would unstop it.

#![allow(clippy::unwrap_used)]

use aether_bloomery::StageId;
use aether_bloomery::testing::digest;
use aether_chassis_bloomery::bloomery::mock_lane::REQUESTED_PATH;
use aether_harness_bloomery::{BloomeryHarness, LaneScript, Oracle};

#[test]
fn a_declining_lane_requests_the_surface_it_needs() {
    let mut harness = BloomeryHarness::start();
    harness.script_lane(
        &aether_bloomery::WorkpieceId("wp".into()),
        StageId::Construct,
        &[LaneScript::DeclineRequestingSurface],
    );
    let bloom = harness.seal_member("wp", digest(0x51));

    harness
        .run_until(|harness| harness.bloom(bloom).members.iter().any(|member| member.awaiting_surface.is_some()), 40);

    let view = harness.bloom(bloom);
    let member = &view.members[0];
    let awaiting = member.awaiting_surface.as_ref().expect("the request reaches the served document");

    // The paths and their reasons are on the document, so an operator reads
    // what to widen without opening an evidence file.
    assert_eq!(awaiting.paths.len(), 1, "the lane asked for exactly what it named: {awaiting:?}");
    assert_eq!(awaiting.paths[0].path, REQUESTED_PATH);
    assert!(!awaiting.paths[0].reason.is_empty(), "a requested path carries the line that justifies it");
    assert_eq!(awaiting.scope_revision, member.scope_revision, "a request is bound to the revision it amends");
    assert_eq!(awaiting.requests, 1);

    // The three park classes stay distinguishable where an operator reads
    // them: a decline *with* a request lights this field and nothing else.
    assert!(member.wedge.is_none(), "asking for surface is not a wedge");
    assert!(member.pending_decision.is_none(), "asking for surface is not an ADR-0151 question");

    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));

    // The park holds: the coordinator does not re-run a refusal a second lap
    // would reproduce verbatim, and no attempt burns while it waits.
    let before = harness.bloom(bloom).members[0].cursor.clone();
    for _ in 0..5 {
        harness.tick();
    }
    let after = harness.bloom(bloom).members[0].clone();
    assert_eq!(after.cursor, before, "a parked member spends no attempt and does not move");
    assert!(after.awaiting_surface.is_some(), "the park persists until it is answered");
    assert!(harness.outstanding().is_empty(), "nothing was dispatched against the parked member");
}
