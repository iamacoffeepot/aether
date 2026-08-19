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
mod harness;
mod lane;

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{
    BloomStatus, BloomView, CONSTRUCT_IMPLEMENT_COMMAND, REVIEW_CRITIC_COMMAND, VERIFY_CHECK_COMMAND, VerifyFailure,
    VerifyFailureSet,
};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use lane::{LaneHarness, while_pumping};

/// Whether the bloom's single member has come to rest either way — resolved, or
/// wedged. The predicate most scenarios wait on, because what separates them is
/// *which* rest they reach, not whether they reach one.
fn at_rest(bloom: &BloomView) -> bool {
    bloom.members.first().is_some_and(|member| member.resolution.is_some() || member.wedge.is_some())
}

/// Drive a bloom whose every lane takes `mode` until the first construct run
/// has recorded, and return the harness.
///
/// Shared by the evidence-shortfall scenarios below. Those used to wait for a
/// wedge; host-fault verdicts now emit `ExecutorFault`, which #5091 admits as
/// a member machinery fault and redispatches on its own axis. Callers that
/// only need "the first run completed" still wait here — the machinery
/// series is bounded by the sealed stage budget, so a missing body cannot
/// loop forever.
fn ran_under(mode: LaneMode) -> LaneHarness {
    let mut harness = LaneHarness::start(&LaneScript::all_passing().with_default(mode));
    harness.wait_for_runs(1);
    harness
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
    assert_eq!(
        bloom.members[0].wedge_cause,
        Some(aether_bloomery::WedgeCause::Work),
        "a clean tree is rejected work, not a machinery series",
    );
    assert!(
        harness.ledger().iter().all(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND),
        "a member that never produced a candidate never reaches its verification",
    );
    harness.assert_live();
}

#[test]
fn a_lane_that_leaves_no_evidence_fails_its_attempt_rather_than_re_driving_forever() {
    // Exit zero, no `evidence.json`. The run is over, so the read will never
    // succeed; re-driving it is an infinite loop, which is what it was. The
    // host now classifies that as ExecutorFault (ADR-0195 §2).
    let harness = ran_under(LaneMode::NoEvidence);
    assert!(!harness.ledger().is_empty(), "the terminal missing body did not loop on the read");
}

#[test]
fn an_empty_evidence_file_fails_its_attempt_rather_than_attesting_nothing() {
    // The full-disk lane, whose wedge digest was the sha256 of the empty
    // string. Zero bytes is not a verdict — it is an unparseable host fault.
    let harness = ran_under(LaneMode::EmptyEvidence);
    assert!(!harness.ledger().is_empty(), "zero-byte evidence completed instead of looping");
}

#[test]
fn evidence_that_does_not_decode_fails_its_attempt() {
    let harness = ran_under(LaneMode::MalformedEvidence);
    assert!(!harness.ledger().is_empty(), "undecodable evidence completed instead of looping");
}

#[test]
fn wrong_nonce_evidence_never_advances_a_member() {
    // A valid-looking body from another run is stale evidence, not a verdict for
    // this order. It must fail closed before construct capture.
    let mut harness = ran_under(LaneMode::MismatchedNonce);
    let bloom = harness.view();
    assert!(bloom.blooms[0].members[0].resolution.is_none(), "a wrong-nonce pass never resolves the member");
    assert!(
        harness.ledger().iter().all(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND),
        "stale construct evidence never captures a candidate or advances to verification",
    );
}

#[test]
fn a_lane_that_exits_non_zero_without_evidence_fails_its_attempt() {
    // The child died before it could judge anything. The host classifies
    // that as ExecutorFault rather than an empty VerificationFailed.
    let harness = ran_under(LaneMode::ExitsNonZero);
    assert!(!harness.ledger().is_empty(), "a lane that died completed instead of looping");
}

#[test]
fn a_missing_evidence_lane_is_a_host_fault_not_a_candidate_failure() {
    // ADR-0195 §2 / #5091: an exited child that wrote no evidence.json
    // rendered no judgment. The backend synthesizes ExecutorFault; intake
    // admits it as a member machinery fault and redispatches the same
    // stage until the sealed Construct budget (2) is gone. That is a
    // bounded series, not an eternal re-drive of the missing file.
    //
    // Wait for both construct dispatches before settle: between rolls the
    // order table is empty, and settle's quiescence window treats that gap
    // as a stop under suite contention.
    let mut harness = LaneHarness::start(&LaneScript::all_passing().with_default(LaneMode::NoEvidence));
    let deadline = Instant::now() + Duration::from_secs(90);
    while harness.ledger().iter().filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND).count() < 2 {
        assert!(
            Instant::now() < deadline,
            "the machinery series never dispatched a second construct; ledger={:?}",
            harness.ledger().iter().map(|run| run.command.as_str()).collect::<Vec<_>>(),
        );
        thread::sleep(Duration::from_millis(250));
    }
    let bloom = harness.settle("the machinery series reaches its ceiling", at_rest);
    let member = &bloom.members[0];
    assert!(member.wedge.is_some(), "the sealed machinery budget ends the series");
    assert_eq!(
        member.wedge_cause,
        Some(aether_bloomery::WedgeCause::Machinery),
        "a host that never judged the candidate is a machinery wedge",
    );
    assert_eq!(member.machinery_rolls, 2, "Construct's sealed budget is two");
    assert_eq!(member.machinery_budget, 2);
    assert_eq!(
        harness.ledger().iter().filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND).count(),
        2,
        "one bounded redispatch, then a terminal stop — not an eternal re-drive",
    );
}

#[test]
fn an_expired_real_process_lane_is_cancelled_as_a_host_fault() {
    // NeverExits writes valid evidence and then parks. Only the sealed
    // deadline can end it. The producer emits ExecutorFault for that
    // expiry; the child must still be reclaimed (no per-order checkout).
    let mut harness =
        LaneHarness::start_with_wall_clock(&LaneScript::all_passing().with_default(LaneMode::NeverExits), 5);
    harness.wait_for_runs(1);
    thread::sleep(Duration::from_secs(7));
    assert_scratch_checkouts_are_lane_slots(&harness, "an expired child leaves no checkout of its own");
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
fn a_preflight_failure_holds_on_the_host_and_resumes_without_refine() {
    // #5020: a missing gate tool is a host fault. The mock's Environment
    // mode on verify.check stamps the production preflight shape; the
    // coordinator must hold at Verify, spend no member budget, and — once
    // the next scripted run can pass — resume on its own cadence without
    // an operator grant or a Refine lap.
    let mut harness = LaneHarness::start(
        &LaneScript::all_passing()
            .then(VERIFY_CHECK_COMMAND, LaneMode::Environment)
            .then(VERIFY_CHECK_COMMAND, LaneMode::Pass),
    );

    let bloom = harness.settle("the member resolves after the host is probed again", |bloom| {
        bloom.members.first().is_some_and(|member| member.resolution.is_some())
    });

    assert!(bloom.members[0].wedge.is_none(), "a host fault must not wedge the member");
    assert!(bloom.members[0].host_fault.is_none(), "a resolved member is no longer held");

    let ledger = harness.ledger();
    let verifies = ledger.iter().filter(|run| run.command == VERIFY_CHECK_COMMAND).count();
    let constructs = ledger.iter().filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND).count();
    assert!(verifies >= 2, "the cadence re-probed Verify after the preflight miss: {ledger:?}");
    assert_eq!(constructs, 1, "no Refine lap for a candidate the gates never judged: {ledger:?}");
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
fn a_lane_that_never_exits_is_cancelled_as_a_host_fault() {
    // A `NeverExits` lane writes a complete, valid `evidence.json` and then
    // parks forever. Only the sealed wall-clock deadline can end it
    // (ADR-0177). The producer emits ExecutorFault for that expiry.
    let mut harness =
        LaneHarness::start_with_wall_clock(&LaneScript::all_passing().with_default(LaneMode::NeverExits), 5);

    harness.wait_for_runs(1);
    thread::sleep(Duration::from_secs(7));
    let bloom = harness.view();
    assert!(bloom.blooms[0].members[0].resolution.is_none(), "a lane that never answered resolves nothing");
    assert_scratch_checkouts_are_lane_slots(&harness, "a cancelled run leaves no checkout of its own behind");
}

#[test]
fn a_lane_that_goes_silent_is_cancelled_before_its_wall_clock() {
    // ADR-0195 §8: a local model lane that streamed progress and then stopped
    // must not occupy the slot until the sealed hour. The host silence
    // threshold cancels it. Intake admits that ExecutorFault as a member
    // machinery fault and redispatches — the load-bearing property here is
    // early cancellation and slot release, not the retry.
    let mut harness =
        LaneHarness::start_with_heartbeat(&LaneScript::all_passing().with_default(LaneMode::NeverExits), 60, 2);

    harness.wait_for_runs(1);
    for nonce in harness.evidence_nonces() {
        harness.write_transcript(&nonce, "{}\n");
    }
    thread::sleep(Duration::from_secs(5));

    let bloom = harness.view();
    assert!(bloom.blooms[0].members[0].resolution.is_none(), "a silent lane resolves nothing");
    assert_scratch_checkouts_are_lane_slots(&harness, "a silenced run leaves no checkout of its own behind");
}

#[test]
fn a_continuously_noisy_lane_still_dies_at_its_absolute_deadline() {
    // Progress only extends the silence window. A lane that keeps writing
    // its transcript must still hit the sealed wall clock — otherwise a
    // noisy but unproductive process would run forever.
    let mut harness =
        LaneHarness::start_with_heartbeat(&LaneScript::all_passing().with_default(LaneMode::NeverExits), 5, 30);
    let runs = harness.runs_dir();
    while_pumping(
        || {
            if let Ok(entries) = fs::read_dir(&runs) {
                for entry in entries.filter_map(Result::ok) {
                    let name = entry.file_name();
                    let Some(nonce) = name.to_str().and_then(|name| name.strip_suffix("-evidence")) else {
                        continue;
                    };
                    let path = entry.path().join("transcript.jsonl");
                    let _ = fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut file| {
                        use std::io::Write as _;
                        file.write_all(nonce.as_bytes())
                    });
                }
            }
        },
        || {
            harness.wait_for_runs(1);
            thread::sleep(Duration::from_secs(7));
        },
    );

    let bloom = harness.view();
    assert!(bloom.blooms[0].members[0].resolution.is_none(), "a noisy hung lane resolves nothing");
    assert_scratch_checkouts_are_lane_slots(&harness, "a deadline-killed noisy run leaves no checkout of its own");
}

// Every checkout git has registered under the harness's run directories.
//
// Both sides are canonicalized: `git worktree list` reports canonical paths, and
// a temp root reached through a symlink (macOS `/var` → `/private/var`) would
// otherwise match nothing and quietly assert over an empty list.
fn registered_scratch_checkouts(harness: &LaneHarness) -> Vec<PathBuf> {
    let runs_dir = fs::canonicalize(harness.runs_dir()).unwrap();
    harness
        .repo()
        .registered_worktrees()
        .into_iter()
        .map(PathBuf::from)
        .filter(|registered| registered.starts_with(&runs_dir))
        .collect()
}

// The scratch checkouts are the lane slots' and nothing else.
//
// A worktree per order, accumulating forever, is the leak this catches — and it
// is invisible to any double mounted above the spawn, because there is no
// worktree to leak. What a dispatch registers is its lane slot's canonical
// checkout (#4904), reused by every dispatch that holds the slot afterwards, so
// the registered set is bounded by the lane ceiling however many orders run.
// Anything named after an order is the leak, and the name is what says so.
fn assert_scratch_checkouts_are_lane_slots(harness: &LaneHarness, context: &str) {
    for checkout in registered_scratch_checkouts(harness) {
        let name = checkout.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_owned();
        assert!(
            name.strip_prefix("slot-").is_some_and(|index| index.chars().all(|digit| digit.is_ascii_digit())),
            "{context}: {name} is not a lane slot's checkout, so something registered one per order",
        );
    }
}

#[test]
fn the_only_scratch_checkouts_a_bloom_leaves_are_its_lane_slots() {
    // The green path's half of the same invariant: a bloom that runs a construct
    // lane, a verify lane, and a critic against one member registers the slot
    // checkouts those dispatches shared, and never a directory per order.
    let mut harness = LaneHarness::start(&LaneScript::all_passing());

    harness
        .settle("the member resolves", |bloom| bloom.members.first().is_some_and(|member| member.resolution.is_some()));

    assert_scratch_checkouts_are_lane_slots(&harness, "a resolved member's dispatches shared their slots' checkouts");
}
