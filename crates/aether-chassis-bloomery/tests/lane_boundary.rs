#![cfg(feature = "github")]

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

use aether_bloomery::{
    BloomStatus, BloomView, CONSTRUCT_IMPLEMENT_COMMAND, REVIEW_CRITIC_COMMAND, VERIFY_CHECK_COMMAND, VerifyFailure,
    VerifyFailureSet,
};
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
    // Four Clippy failures reach Verify's three-repeat ceiling: the first is
    // novel and free, then each recurrence spends one repair roll. Keep every
    // Refine run passing so this wedges at Verify, not at the repair stage.
    let mut harness = LaneHarness::start(
        &LaneScript::all_passing()
            .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
            .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
            .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
            .then(VERIFY_CHECK_COMMAND, LaneMode::Fail),
    );

    let bloom =
        harness.settle("the member wedges", |bloom| bloom.members.first().is_some_and(|member| member.wedge.is_some()));

    let wedge = bloom.members[0].wedge.as_ref().expect("the settle predicate held");
    assert_ne!(
        wedge.evidence,
        aether_bloomery::Digest::default(),
        "a wedge must name the evidence that produced it, or the halt is unaccountable",
    );
    assert_eq!(
        wedge.repeated_verifiers,
        VerifyFailureSet::one(VerifyFailure::Clippy),
        "the fixture's repeated clippy failures are the exact terminal accounting set",
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
fn wrong_nonce_evidence_never_advances_a_member_or_leaves_an_order_outstanding() {
    // A valid-looking body from another run is stale evidence, not a verdict for
    // this order. It must fail closed before construct capture, then exhaust the
    // normal retry budget into an accountable wedge.
    let (mut harness, bloom) = rest_under(LaneMode::MismatchedNonce);

    assert!(bloom.members[0].resolution.is_none(), "a wrong-nonce pass never resolves the member");
    assert!(bloom.members[0].wedge.is_some(), "wrong-nonce attempts reach the normal wedge counter");
    assert!(harness.outstanding().is_empty(), "each rejected body consumes its order rather than stalling the lane");
    assert!(
        harness.ledger().iter().all(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND),
        "stale construct evidence never captures a candidate or advances to verification",
    );
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
    // The mode #4730 called "review-finds-on-clean-tree", and the first
    // scenario here that outlives the member line: a critic's finding lands
    // against the *fold*, so a member that already resolved has to re-open and
    // the second fold has to carry the repair. Everything behind
    // Integrate→AggregateVerify→AggregateReview needs a GitHub, which the
    // fixture backend supplies (#4732) — and needs it to mint real git objects,
    // because the aggregate lanes check the fold out through the same real `git
    // worktree add` every member lane uses.
    let mut harness = LaneHarness::start(&LaneScript::all_passing().then(REVIEW_CRITIC_COMMAND, LaneMode::Fail));

    // Two waits rather than one: the member coming to rest and the bloom
    // resolving are distinct milestones, so a stall names which of them it
    // stopped short of.
    harness.settle("the member comes to rest", at_rest);
    let bloom = harness.settle("the bloom resolves after its aggregate repair lap", |bloom| {
        matches!(bloom.status, BloomStatus::Resolved | BloomStatus::Landed)
    });

    assert!(bloom.members[0].wedge.is_none(), "an aggregate finding inside the budget wedges nothing");
    let critics = harness.ledger().into_iter().filter(|run| run.command == REVIEW_CRITIC_COMMAND).count();
    assert!(critics >= 2, "the finding re-drove a critic over the repaired fold rather than being dropped: {critics}");
    harness.assert_live();
}

#[test]
fn an_aggregate_review_that_cannot_run_retries_the_review_and_never_opens_a_repair_lap() {
    // ADR-0176, end to end below the spawn seam: the critic lane reports that it
    // could not execute at all, and the whole chain — the lane's stamped
    // `environment`, the backend's verdict derivation, the intake's stage
    // binding, the reducer's fault ledger — has to carry that as a host fault.
    //
    // Every assertion below failed before this chain existed: the fault
    // flattened to `status: fail`, so the member that had already resolved was
    // re-opened into Refine against a candidate no critic ever read, and a lap
    // of its bounded repair budget was spent on a broken sandbox.
    //
    // Only the critic faults: the member line runs green, so the fold under
    // review is one every gate before the critic already accepted. Both faults
    // take that same fold, so the second reaches AggregateReview's sealed budget
    // of two and the series is terminal.
    let mut harness = LaneHarness::start(
        &LaneScript::all_passing()
            .then(REVIEW_CRITIC_COMMAND, LaneMode::Environment)
            .then(REVIEW_CRITIC_COMMAND, LaneMode::Environment),
    );

    harness.settle("the member comes to rest", at_rest);
    let bloom = harness.settle("the executor-fault series reaches its ceiling", |bloom| {
        bloom.executor_fault.is_some_and(|fault| fault.terminal)
    });

    let fault = bloom.executor_fault.expect("the settle predicate held");
    assert_eq!((fault.rolls, fault.budget), (2, 2), "the series is bounded by the sealed AggregateReview budget");
    assert_ne!(
        fault.evidence,
        aether_bloomery::Digest::default(),
        "a terminal fault must name the report that produced it, or the halt is unaccountable",
    );
    assert_eq!(bloom.status, BloomStatus::Sealed, "a bloom that was never judged neither resolves nor lands");

    // The whole point: no member paid for it.
    assert!(bloom.members[0].resolution.is_some(), "the member's claim survives an outage it did not cause");
    assert!(bloom.members[0].wedge.is_none(), "and it is not the one that wedged");

    // One redispatch below the ceiling and none at it: exactly two critic runs,
    // and no second construct lap, which is what a repair re-entry would leave.
    let ledger = harness.ledger();
    let critics = ledger.iter().filter(|run| run.command == REVIEW_CRITIC_COMMAND).count();
    let constructs = ledger.iter().filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND).count();
    assert_eq!(critics, 2, "one bounded redispatch of the review, then a terminal stop: {ledger:?}");
    assert_eq!(constructs, 1, "no member re-entered Refine for a fault it did not cause: {ledger:?}");

    // And the stop is an accountable one, not a stall and not a quiet finish.
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
