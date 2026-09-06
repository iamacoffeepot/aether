//! Configuring a second member or stage used to replace the whole mock-lane
//! script, so a scenario could pass without exercising its declared failure.
//!
//! Pre-fix: `script_lane` rebuilt one global script from `all_passing` and
//! discarded the workpiece. Member B's Construct Candidate overwrote member
//! A's Construct Decline; both members then took passing behaviour. A later
//! Verify Fail likewise erased an earlier Construct Decline, so the member
//! produced a candidate instead of parking.

#![allow(clippy::unwrap_used)]

use aether_bloomery::testing::digest;
use aether_bloomery::{MemberView, StageId, WorkpieceId};
use aether_harness_bloomery::{BloomeryHarness, LaneScript, Oracle};

fn named<'a>(members: &'a [MemberView], workpiece: &str) -> &'a MemberView {
    members.iter().find(|member| member.workpiece.0 == workpiece).unwrap_or_else(|| panic!("no member {workpiece}"))
}

#[test]
fn a_later_stage_script_does_not_erase_an_earlier_stage_fault() {
    let mut harness = BloomeryHarness::start();
    harness.script_lane(&WorkpieceId("wp".into()), StageId::Construct, &[LaneScript::Decline]);
    harness.script_lane(&WorkpieceId("wp".into()), StageId::Verify, &[LaneScript::Die]);
    let bloom = harness.seal_member("wp", digest(0x51));
    harness.run_until(|harness| harness.bloom(bloom).members.iter().any(|member| member.park.is_some()), 40);

    let member = &harness.bloom(bloom).members[0];
    assert!(member.park.is_some(), "the construct decline still parks after a later stage is scripted: {member:?}");
    assert!(member.wedge.is_none(), "a park is not a verify-fail wedge");
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}

#[test]
fn a_second_member_script_does_not_erase_the_first_members_fault() {
    let mut harness = BloomeryHarness::start();
    let scope_a = harness.author_scope_revision("wp-a", &["crates/example-a/**"]);
    let scope_b = harness.author_scope_revision("wp-b", &["crates/example-a/**"]);
    harness.script_lane(&WorkpieceId("wp-a".into()), StageId::Construct, &[LaneScript::Decline]);
    harness.script_lane(
        &WorkpieceId("wp-b".into()),
        StageId::Construct,
        &[LaneScript::DeclineRequestingSurface],
    );
    let bloom = harness.seal_members(&[("wp-a", scope_a), ("wp-b", scope_b)]);
    harness.run_until(
        |harness| {
            let view = harness.bloom(bloom);
            view.members.iter().any(|member| member.workpiece.0 == "wp-a" && member.park.is_some())
                && view.members.iter().any(|member| member.workpiece.0 == "wp-b" && member.awaiting_surface.is_some())
        },
        60,
    );

    let view = harness.bloom(bloom);
    let member_a = named(&view.members, "wp-a");
    let member_b = named(&view.members, "wp-b");
    assert!(member_a.park.is_some(), "member A's decline survived member B's later script: {member_a:?}");
    assert!(member_a.awaiting_surface.is_none(), "A was not given B's surface-requesting decline: {member_a:?}");
    assert!(member_b.awaiting_surface.is_some(), "member B kept the fault it was scripted: {member_b:?}");
    assert!(member_b.park.is_none(), "B was not given A's request-less park: {member_b:?}");
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}
