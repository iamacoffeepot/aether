#![allow(clippy::print_stderr)]
//! Lane-boundary scenarios (#4727): a real coordinator driven through a real
//! `git worktree add` and a real lane subprocess, with the mock lane binary as
//! the only substitution.
//!
//! Every scenario here checks the liveness invariants as a side effect of
//! waiting — see `lane::liveness`. A scenario asserts the behaviour it is named
//! for; the invariants assert the thing nobody predicted.
//!
//! Unix only: the harness's coordinator guard reaps through signals the
//! substrate does not model elsewhere (ADR-0049 §7), and the scenarios fork a
//! process per dispatch.

#![cfg(unix)]
#![allow(clippy::unwrap_used, reason = "a scenario that cannot set up its coordinator reports it by panicking")]

mod common;
mod lane;

use std::path::Path;

use aether_bloomery::{BloomView, CONSTRUCT_IMPLEMENT_COMMAND, REVIEW_CRITIC_COMMAND, VERIFY_CHECK_COMMAND};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use lane::LaneHarness;

/// Whether the bloom's single member has come to rest either way — resolved, or
/// wedged. The predicate most scenarios wait on, because what separates them is
/// *which* rest they reach, not whether they reach one.
fn at_rest(bloom: &BloomView) -> bool {
    bloom.members.first().is_some_and(|member| member.resolution.is_some() || member.wedge.is_some())
}

/// Drive a bloom whose every lane takes `mode` until its member comes to rest,
/// and return the harness plus that member's rest state.
///
/// Shared by the evidence-shortfall scenarios below, which differ only in the
/// shortfall the lane commits: each asserts the same thing — a lane that leaves
/// unreadable evidence must fail its attempt, never re-drive forever — and the
/// mode is the whole of what distinguishes them.
fn rest_under(mode: LaneMode) -> (LaneHarness, BloomView) {
    let mut harness = LaneHarness::start(&LaneScript::all_passing().with_default(mode));
    let bloom = harness.settle("the member comes to rest", at_rest);
    (harness, bloom)
}

#[test]
fn a_bloom_whose_lanes_all_pass_resolves_its_member() {
    // The green path, end to end below the spawn seam: a construct lane writes
    // a candidate into a worktree `git worktree add` materialized, the
    // coordinator captures and commits it, and the mechanical and critic lanes
    // that follow judge it. Everything but the program at the end of the argv is
    // the production path.
    let mut harness = LaneHarness::start(&LaneScript::all_passing());

    let bloom = harness
        .settle("the member resolves", |bloom| bloom.members.first().is_some_and(|member| member.resolution.is_some()));

    assert!(bloom.members[0].wedge.is_none(), "a green lap wedges nothing");
    let commands: Vec<String> = harness.ledger().into_iter().map(|run| run.command).collect();
    assert!(
        commands.contains(&CONSTRUCT_IMPLEMENT_COMMAND.to_owned()),
        "the construct lane ran as a real subprocess: {commands:?}",
    );
    assert!(commands.contains(&VERIFY_CHECK_COMMAND.to_owned()), "the verify lane ran too: {commands:?}");
}

#[test]
fn a_bloom_that_fails_verification_twice_still_resolves() {
    // Refine re-entry across three separate lane processes. The script is
    // consumed through an on-disk ledger precisely so this works: each dispatch
    // is a fresh process that must read the *next* step, not the first.
    let mut harness = LaneHarness::start(
        &LaneScript::all_passing()
            .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
            .then(VERIFY_CHECK_COMMAND, LaneMode::Fail),
    );

    let bloom = harness.settle("the member resolves after two failed verifications", |bloom| {
        bloom.members.first().is_some_and(|member| member.resolution.is_some())
    });

    assert!(bloom.members[0].wedge.is_none(), "two failures inside the budget wedge nothing");
    let verifies = harness.ledger().into_iter().filter(|run| run.command == VERIFY_CHECK_COMMAND).count();
    assert!(verifies >= 3, "the failing verifications re-drove rather than being retried in place: {verifies} runs");
}

#[test]
fn a_candidate_that_does_not_build_steers_its_repair_lap_with_the_diagnostics() {
    // A failing verification is only useful if what it found reaches the lane
    // that has to fix it. The findings travel from one child's `evidence.json`,
    // through the coordinator's persistence, into the next child's `--task` —
    // a path that exists entirely below the spawn seam, so nothing above it can
    // observe whether it is connected.
    let mut harness = LaneHarness::start(&LaneScript::all_passing().then(VERIFY_CHECK_COMMAND, LaneMode::Fail));

    harness.settle("the member resolves after its repair lap", |bloom| {
        bloom.members.first().is_some_and(|member| member.resolution.is_some())
    });

    let repair = harness
        .ledger()
        .into_iter()
        .filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND)
        .nth(1)
        .expect("the failing verification re-entered a repair lap");
    let task = repair.task.expect("a repair lap is handed an advisory work order");
    assert!(task.contains("E0308"), "the failing gate's diagnostics steer the repair, not a blind re-roll: {task}");
}

#[test]
fn a_member_whose_verification_never_passes_wedges_with_recorded_evidence() {
    // The ceiling. What matters is not that it stops but that it stops
    // *accountably* — a wedge naming the evidence that produced it, which is
    // what separates a recorded halt from the silent one this harness exists to
    // catch.
    let mut harness = LaneHarness::start(&LaneScript::all_passing().with_default(LaneMode::Fail));

    let bloom =
        harness.settle("the member wedges", |bloom| bloom.members.first().is_some_and(|member| member.wedge.is_some()));

    let wedge = bloom.members[0].wedge.as_ref().expect("the settle predicate held");
    assert_ne!(
        wedge.evidence,
        aether_bloomery::Digest::default(),
        "a wedge must name the evidence that produced it, or the halt is unaccountable",
    );
    harness.assert_live();
}

#[test]
fn a_construct_run_that_writes_nothing_does_not_advance_the_member() {
    // The empty candidate: the lane claims it produced one and leaves the
    // worktree clean. The claim is in the evidence, so only the capture can
    // catch it — which is exactly the step a runner-level double skips.
    let mut harness = LaneHarness::start(&LaneScript::all_passing().with_default(LaneMode::ConcludesWithoutWriting));

    let bloom = harness.settle("the empty candidate does not advance the member", at_rest);

    assert!(
        bloom.members[0].resolution.is_none(),
        "a construct run with nothing behind its claim must not resolve a member",
    );
    assert!(bloom.members[0].wedge.is_some(), "and the failed captures must reach the wedge counter");
    assert!(
        harness.ledger().iter().all(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND),
        "a member that never produced a candidate never reaches its verification",
    );
    harness.assert_live();
}

#[test]
fn a_lane_that_leaves_no_evidence_fails_its_attempt_rather_than_re_driving_forever() {
    // Exit zero, no `evidence.json`. The run is over, so the read will never
    // succeed; re-driving it is an infinite loop, which is what it was.
    let (mut harness, bloom) = rest_under(LaneMode::NoEvidence);

    assert!(bloom.members[0].wedge.is_some(), "an unreadable attempt must reach the wedge counter");
    harness.assert_live();
}

#[test]
fn an_empty_evidence_file_fails_its_attempt_rather_than_attesting_nothing() {
    // The full-disk lane, whose wedge digest was the sha256 of the empty
    // string. Zero bytes is not a verdict.
    let (mut harness, bloom) = rest_under(LaneMode::EmptyEvidence);

    assert!(bloom.members[0].wedge.is_some(), "zero-byte evidence must fail closed");
    harness.assert_live();
}

#[test]
fn evidence_that_does_not_decode_fails_its_attempt() {
    let (mut harness, bloom) = rest_under(LaneMode::MalformedEvidence);

    assert!(bloom.members[0].wedge.is_some(), "undecodable evidence must fail closed");
    harness.assert_live();
}

#[test]
fn a_lane_that_exits_non_zero_without_evidence_fails_its_attempt() {
    // An environment failure rather than a candidate failure: the child died
    // before it could judge anything. It still has to reach the counter, or the
    // bloom stalls with no record of why.
    let (mut harness, bloom) = rest_under(LaneMode::ExitsNonZero);

    assert!(bloom.members[0].wedge.is_some(), "a lane that died must reach the wedge counter");
    harness.assert_live();
}

#[test]
fn a_failing_aggregate_review_drives_a_repair_lap_that_resolves() {
    // The mode #4730 called "review-finds-on-clean-tree" — today's death:
    // an aggregate-review finding drives a repair lap whose second
    // integration must resolve. Needs the fake-GitHub seam (#4732) because
    // the aggregate line is behind Integrate→AggregateVerify→AggregateReview.
    let mut harness = LaneHarness::start(&LaneScript::all_passing().then(REVIEW_CRITIC_COMMAND, LaneMode::Fail));

    // First wait for at_rest to see the member line.
    let bloom = harness.settle("aggregate repair at_rest", |bloom| {
        bloom.members.first().is_some_and(|m| m.resolution.is_some() || m.wedge.is_some())
    });
    eprintln!("bloom status after at_rest: {:?}", bloom.status);
    eprintln!("member wedge: {:?}", bloom.members[0].wedge);
    let ledger = harness.ledger();
    eprintln!("ledger: {:?}", ledger.iter().map(|r| (r.command.clone(), r.mode, r.nonce.clone())).collect::<Vec<_>>());
    eprintln!("view: {:?}", harness.view());
    // Dump outbox for debugging
    {
        let store_path = harness.store_path();
        let conn =
            rusqlite::Connection::open_with_flags(&store_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = conn.prepare("SELECT topic, sequence, hex(payload) FROM outbox ORDER BY sequence").unwrap();
        let rows: Vec<(String, i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0).unwrap(), row.get(1).unwrap(), row.get(2).unwrap())))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        eprintln!("outbox rows: {:?}", rows.iter().map(|(t, s, _)| (t.clone(), *s)).collect::<Vec<_>>());
        let mut stmt2 = conn.prepare("SELECT nonce, hex(bloom), workpiece FROM outstanding_orders").unwrap();
        let outstanding: Vec<(String, String, String)> = stmt2
            .query_map([], |row| Ok((row.get(0).unwrap(), row.get(1).unwrap(), row.get(2).unwrap())))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        eprintln!("outstanding_orders: {outstanding:?}");
    }
    // Now wait for the aggregate line to finish
    let bloom2 = harness.settle("the bloom resolves after aggregate repair", |bloom| {
        matches!(bloom.status, aether_bloomery::BloomStatus::Resolved | aether_bloomery::BloomStatus::Landed)
    });
    eprintln!("bloom2 status: {:?}", bloom2.status);
    assert!(bloom2.members[0].wedge.is_none(), "aggregate finding within budget must not wedge");
    harness.assert_live();
}

#[test]
fn every_dispatch_releases_the_scratch_worktree_it_materialized() {
    // A worktree per order, forever, is the leak the release path exists to
    // prevent — and it is invisible to any double mounted above the spawn,
    // because there is no worktree to leak.
    let mut harness = LaneHarness::start(&LaneScript::all_passing());

    harness
        .settle("the member resolves", |bloom| bloom.members.first().is_some_and(|member| member.resolution.is_some()));

    let runs_dir = harness.runs_dir();
    let leaked: Vec<String> = harness
        .repo()
        .registered_worktrees()
        .into_iter()
        .filter(|registered| Path::new(registered).starts_with(&runs_dir))
        .collect();
    assert!(leaked.is_empty(), "every consumed run released its scratch worktree; leaked: {leaked:?}");
}
