//! Configuring a second member or stage used to replace the whole mock-lane
//! script, so a scenario could pass without exercising its declared failure.
//!
//! Pre-fix: `script_lane` rebuilt one global script from `all_passing` and
//! discarded the workpiece. Member B's Construct Candidate overwrote member
//! A's Construct Decline; both members then took passing behaviour. A later
//! Verify Fail likewise erased an earlier Construct Decline, so the member
//! produced a candidate instead of parking. Construct and Refine share
//! `construct.implement`, so a Refine script also erased Construct.

#![allow(clippy::unwrap_used)]

use aether_bloomery::testing::digest;
use aether_bloomery::{BloomId, MemberView, StageId, VerifyFailureSet, WorkpieceId};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneRun};
use aether_harness_bloomery::{BloomeryHarness, LaneScript, Oracle, Progress, is_answerable};

fn named<'a>(members: &'a [MemberView], workpiece: &str) -> &'a MemberView {
    members.iter().find(|member| member.workpiece.0 == workpiece).unwrap_or_else(|| panic!("no member {workpiece}"))
}

// The named stops Excuse::ALL counts — not host_fault alone.
// Die/missing-evidence admits MemberExecutorFault (machinery retry or
// wedge). Projection host_fault is only the preflight VerifyHostFault hold.
fn named_stop(member: &MemberView) -> bool {
    member.resolution.is_some()
        || member.wedge.is_some()
        || member.host_fault.is_some()
        || member.park.is_some()
        || member.awaiting_surface.is_some()
        || member.evicted_by.is_some()
        || member.withdrawn.is_some()
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
    // Bare digests: no work-order `--task` pin. Distinction has to come from
    // the journaled order identity the spawn seam records.
    let mut harness = BloomeryHarness::start();
    harness.script_lane(&WorkpieceId("wp-a".into()), StageId::Construct, &[LaneScript::Decline]);
    harness.script_lane(&WorkpieceId("wp-b".into()), StageId::Construct, &[LaneScript::DeclineRequestingSurface]);
    let bloom = harness.seal_members(&[("wp-a", digest(0x51)), ("wp-b", digest(0x52))]);
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

#[test]
fn two_members_verify_keep_distinct_faults_without_a_task_header() {
    // Distinct environment-vs-judged Verify outcomes so a global script
    // overwrite collapses their modes. Judged Fail dispatches Refine; that
    // member's Refine is declined so the stop is named rather than walking
    // into a sibling wait with no excuse.
    let mut harness = BloomeryHarness::start();
    harness.script_lane(&WorkpieceId("wp-a".into()), StageId::Verify, &[LaneScript::Die]);
    harness.script_lane(
        &WorkpieceId("wp-b".into()),
        StageId::Verify,
        &[LaneScript::VerifyFail(VerifyFailureSet::EMPTY)],
    );
    harness.script_lane(&WorkpieceId("wp-b".into()), StageId::Refine, &[LaneScript::Decline]);
    let bloom = harness.seal_members(&[("wp-a", digest(0x51)), ("wp-b", digest(0x52))]);
    wait_until_distinct_verify_faults(&mut harness, bloom, 80);

    let ledger = harness.ledger();
    assert_eq!(verify_mode(&ledger, "wp-a"), Some(LaneMode::ExitsNonZero), "A's verify Die survived B's later script");
    assert_eq!(verify_mode(&ledger, "wp-b"), Some(LaneMode::Fail), "B's verify Fail was not consumed by A's Die");
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{violation}"));
}

fn wait_until_distinct_verify_faults(harness: &mut BloomeryHarness, bloom: BloomId, ticks: u32) {
    let mut last = None;
    let mut still = 0_u32;
    for _ in 0..ticks {
        harness.tick();
        let progress = Progress::observe(&harness.view(), harness.outstanding(), harness.ledger().len());
        if last.as_ref() == Some(&progress) {
            still += 1;
        } else {
            last = Some(progress.clone());
            still = 0;
        }
        if still >= 2 && is_answerable(&progress) {
            check_oracle(harness, "");
        }
        let ledger = harness.ledger();
        let view = harness.bloom(bloom);
        if verify_mode(&ledger, "wp-a") == Some(LaneMode::ExitsNonZero)
            && verify_mode(&ledger, "wp-b") == Some(LaneMode::Fail)
            && view.members.iter().all(named_stop)
        {
            let progress = Progress::observe(&harness.view(), harness.outstanding(), harness.ledger().len());
            if is_answerable(&progress) {
                check_oracle(harness, "");
            }
            return;
        }
    }
    check_oracle(harness, "tick budget exhausted: ");
    let view = harness.bloom(bloom);
    let members: Vec<_> = view
        .members
        .iter()
        .map(|member| {
            format!(
                "{} resolution={} wedge={} host_fault={} park={} awaiting_surface={} evicted_by={} machinery_rolls={}",
                member.workpiece.0,
                member.resolution.is_some(),
                member.wedge.is_some(),
                member.host_fault.is_some(),
                member.park.is_some(),
                member.awaiting_surface.is_some(),
                member.evicted_by.is_some(),
                member.machinery_rolls,
            )
        })
        .collect();
    let ledger: Vec<_> = harness
        .ledger()
        .iter()
        .map(|run| format!("{:?} {:?} {:?} {:?}", run.workpiece, run.stage, run.command, run.mode))
        .collect();
    panic!("predicate not reached inside {ticks} ticks; members={members:?} ledger={ledger:?}");
}

fn check_oracle(harness: &mut BloomeryHarness, context: &str) {
    harness.doctor_tick();
    Oracle::check(&harness.view(), harness.doctor().as_ref(), &harness.outstanding())
        .unwrap_or_else(|violation| panic!("{context}{violation}"));
}

#[test]
fn a_refine_script_does_not_replace_construct_on_the_shared_command() {
    let mut harness = BloomeryHarness::start();
    harness.script_lane(&WorkpieceId("wp".into()), StageId::Construct, &[LaneScript::Candidate]);
    harness.script_lane(&WorkpieceId("wp".into()), StageId::Refine, &[LaneScript::Decline]);
    let _bloom = harness.seal_member("wp", digest(0x51));
    harness.run_until(
        |harness| {
            harness
                .ledger()
                .iter()
                .any(|run| run.stage == Some(StageId::Construct) && run.workpiece.as_deref() == Some("wp"))
        },
        40,
    );

    let construct = harness
        .ledger()
        .into_iter()
        .find(|run| run.stage == Some(StageId::Construct) && run.workpiece.as_deref() == Some("wp"))
        .expect("construct dispatched");
    assert_eq!(
        construct.mode,
        LaneMode::Pass,
        "Construct Candidate must not become Refine Decline on the shared command: {construct:?}"
    );
}

fn verify_mode(ledger: &[LaneRun], workpiece: &str) -> Option<LaneMode> {
    ledger
        .iter()
        .find(|run| run.stage == Some(StageId::Verify) && run.workpiece.as_deref() == Some(workpiece))
        .map(|run| run.mode)
}
