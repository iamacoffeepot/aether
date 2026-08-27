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

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use aether_bloomery::{
    BloomStatus, BloomView, CONSTRUCT_IMPLEMENT_COMMAND, REVIEW_CRITIC_COMMAND, VERIFY_CHECK_COMMAND, VerifyFailure,
    VerifyFailureSet,
};
use aether_chassis_bloomery::bloomery::admits_lane_key;
use aether_chassis_bloomery::bloomery::mock_lane::{FOREIGN_SESSION_ID, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{SqliteStore, StoreBackend};
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, LaneHarness, while_pumping};

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
fn a_conversation_keeps_its_session_tree_and_a_mechanical_lane_builds_in_its_slot() {
    // Acceptance for #5425 as amended by the slot-pairing fix, below the spawn
    // seam. Two halves, one rule per lane class:
    //
    // A *conversation* lane stands in the tree its session opened, every lap. A
    // harness binds a conversation permanently to the directory it was born in —
    // grok stores sessions under a percent-encoded working directory and ignores
    // `--cwd` on a resume — so a member whose launches landed in different slots
    // had its resumed lap edit whatever was in the old slot while its own
    // checkout stayed clean, which reads downstream as a lane that produced
    // nothing (dispatch-2374, dispatch-2379).
    //
    // A *mechanical* lane carries no conversation, and a per-session tree is a
    // path no earlier lane ever built at — cargo keys freshness on mtimes a
    // fresh worktree restamps and sccache keys on the path itself — so the
    // verify that judges the candidate builds in its slot's own checkout, where
    // the paths are the ones that slot has always built at.
    let mut harness = LaneHarness::start_with(&LaneScript::all_passing(), "wp-own-tree");
    harness
        .settle("the member resolves", |bloom| bloom.members.first().is_some_and(|member| member.resolution.is_some()));

    let sessions = harness.runs_dir().join("sessions");
    let placed: Vec<(String, String)> =
        harness.ledger().into_iter().filter_map(|run| run.worktree.map(|worktree| (run.command, worktree))).collect();

    let session_lanes: Vec<&(String, String)> =
        placed.iter().filter(|(_, worktree)| worktree.contains("/sessions/")).collect();
    let commands: BTreeSet<&str> = session_lanes.iter().map(|(command, _)| command.as_str()).collect();
    assert!(commands.contains(CONSTRUCT_IMPLEMENT_COMMAND), "the conversation lane ran in a session tree: {placed:?}");
    let trees: BTreeSet<&str> = session_lanes.iter().map(|(_, worktree)| worktree.as_str()).collect();
    assert_eq!(trees.len(), 1, "every conversation launch stood in one tree: {session_lanes:?}");
    let tree = Path::new(trees.iter().next().unwrap());
    assert_eq!(tree.file_name().and_then(|name| name.to_str()), Some("tree"));
    assert_eq!(tree.parent().and_then(Path::parent), Some(sessions.as_path()), "and it is a session's: {tree:?}");

    let verify_trees: Vec<&(String, String)> =
        placed.iter().filter(|(command, _)| command == VERIFY_CHECK_COMMAND).collect();
    assert!(!verify_trees.is_empty(), "the mechanical verify ran with a placed tree: {placed:?}");
    for (_, worktree) in &verify_trees {
        assert!(
            !worktree.contains("/sessions/") && worktree.contains("slot-"),
            "a mechanical lane builds in its slot, not a session tree: {verify_trees:?}",
        );
    }

    let session_dirs: Vec<String> = fs::read_dir(&sessions)
        .expect("the session root exists")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(session_dirs.len(), 1, "one member's line opened one session: {session_dirs:?}");
}

/// The coordinator knobs the lane harness forks its coordinator under — the
/// journal, the artifact store, the repository binding, the poll cadence, and
/// above all the two live control ports (`support::process` sets `AETHER_RPC_PORT`
/// and `AETHER_HTTP_PORT` itself). Every one is a knob a chassis forked *under* a
/// lane would resolve as its own.
const COORDINATOR_KNOBS: [&str; 6] = [
    "AETHER_RPC_PORT",
    "AETHER_HTTP_PORT",
    "AETHER_STORE_PATH",
    "AETHER_ARTIFACTS_ROOT",
    "AETHER_GITHUB_BACKEND",
    "AETHER_GITHUB_POLL_INTERVAL_SECS",
];

/// Names a process's own runtime stamps into its environment whatever its parent
/// handed it: macOS's `CoreFoundation` writes `__CF_USER_TEXT_ENCODING` into every
/// process it initializes in. Their presence in a child says nothing about what
/// crossed the boundary, so the containment check steps over them rather than
/// reading a platform's own bookkeeping as a leak.
const SELF_STAMPED: [&str; 1] = ["__CF_USER_TEXT_ENCODING"];

#[test]
fn a_lane_child_comes_up_on_a_constructed_environment_not_the_coordinators() {
    // The incident this prevents: on 2026-08-25 a base verify recorded forty
    // full-suite failures whose in-lane solo replays failed too, while the
    // identical nextest invocation passed instantly from a clean shell. The lane
    // child had inherited the coordinator's whole process environment — twenty-two
    // AETHER_* runtime variables on the production host, the live control and RPC
    // ports among them — so every test under it that forks a chassis (the fleet
    // harness suites, the hub binary-store proofs, the http serving tests)
    // resolved the coordinator's ports as the ones to bind, and either failed to
    // bind or dialled the live coordinator. It read for months as "fleetharness
    // flakes under saturation" (#5475), because a deterministic poisoning that
    // only fires when a coordinator is up looks exactly like a flake.
    //
    // Nothing about this scenario is simulated: the coordinator is a forked
    // `bloomery` process holding real knobs, the lane is a real child of it, and
    // what the child came up holding is what it recorded. Only the program at the
    // end of the argv is a stand-in.
    let mut harness = LaneHarness::start(&LaneScript::all_passing());
    harness
        .settle("the member resolves", |bloom| bloom.members.first().is_some_and(|member| member.resolution.is_some()));

    let runs = harness.ledger();
    assert!(!runs.is_empty(), "the scenario has to have dispatched a real lane to have an environment to judge");
    for run in &runs {
        let child: BTreeSet<&str> = run.env.iter().map(String::as_str).collect();
        assert!(
            child.contains("PATH"),
            "{}: a lane whose PATH did not cross could not resolve cargo, git, or its harness — the over-scrub that \
             would be a louder bug than the leak: {child:?}",
            run.command,
        );
        for knob in COORDINATOR_KNOBS {
            assert!(!child.contains(knob), "{}: the coordinator's {knob} reached its lane child", run.command);
        }
        let leaked: Vec<&&str> =
            child.iter().filter(|key| !admits_lane_key(OsStr::new(key)) && !SELF_STAMPED.contains(key)).collect();
        assert!(
            leaked.is_empty(),
            "{}: the child's environment is constructed from the allow list, so nothing outside it can appear: \
             {leaked:?}",
            run.command,
        );
    }

    // And the denial was load-bearing rather than vacuous: this process is the
    // forked coordinator's own parent, so what it carries is what the coordinator
    // carried and the lane would have inherited wholesale.
    assert!(
        env::vars_os()
            .any(|(key, _)| !admits_lane_key(&key) && !SELF_STAMPED.contains(&key.to_string_lossy().as_ref())),
        "the ancestry carried nothing to deny, so the scenario proved nothing",
    );
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
fn wrong_nonce_evidence_leaves_no_session_behind() {
    // The same unbound body used to file its session id as this member's and
    // deposit it in the pool before the nonce gate ran. A later refine then
    // resumed a conversation that belonged to another lane.
    let roots = HarnessRoots::create();
    let mut harness = HarnessBuilder::lane(&LaneScript::all_passing().with_default(LaneMode::MismatchedNonce))
        .roots(&roots)
        .start("wrong-nonce-session");
    let deadline = Instant::now() + Duration::from_secs(90);
    while harness.ledger().iter().filter(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND).count() < 2 {
        assert!(
            Instant::now() < deadline,
            "the machinery series never dispatched a second construct; ledger={:?}",
            harness.ledger().iter().map(|run| run.command.as_str()).collect::<Vec<_>>(),
        );
        thread::sleep(Duration::from_millis(250));
    }

    let bloom = harness.view();
    let bloom_id = bloom.blooms[0].id;
    let mut store = SqliteStore::open(&roots.store_path()).expect("the journal opens");
    assert_eq!(
        store.lookup_construct_session(bloom_id.0.as_bytes(), "wp").expect("the construct-session row reads"),
        None,
        "an unbound body must not file a construct session for the member",
    );

    let store_path = roots.store_path();
    let parent = Path::new(&store_path).parent().expect("the journal has a directory");
    let deposited = ["sessions.sqlite", "sessions.sqlite-wal"].iter().any(|name| {
        fs::read(parent.join(name)).is_ok_and(|bytes| {
            bytes.windows(FOREIGN_SESSION_ID.len()).any(|window| window == FOREIGN_SESSION_ID.as_bytes())
        })
    });
    assert!(!deposited, "an unbound body must not deposit its session in the pool");
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
    assert_scratch_checkouts_are_named_for_work(&harness, "an expired child leaves no checkout of its own");
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
    assert_scratch_checkouts_are_named_for_work(&harness, "a cancelled run leaves no checkout of its own behind");
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
    let nonces = harness.evidence_nonces();
    for nonce in &nonces {
        harness.write_transcript(nonce, "{}\n");
    }
    // Wait until the original nonce leaves outstanding: a cancelled run is
    // admitted as a host fault and redispatched under a new nonce. The
    // executor tick is the GitHub poll floor, longer than the 2 s allowance.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let outstanding = harness.outstanding();
        if nonces.iter().all(|nonce| !outstanding.contains(nonce)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a silent lane's nonce leaves outstanding rather than occupying the slot; outstanding={outstanding:?} original={nonces:?}"
        );
        thread::sleep(Duration::from_millis(200));
    }

    let bloom = harness.view();
    assert!(bloom.blooms[0].members[0].resolution.is_none(), "a silent lane resolves nothing");
    assert_scratch_checkouts_are_named_for_work(&harness, "a silenced run leaves no checkout of its own behind");
}

#[test]
fn a_lane_beating_only_its_heartbeat_is_not_silence() {
    // The transcript falls silent the moment the model ends its turn, and the
    // lane then stamps `heartbeat` while it compiles. Pre-fix this cancelled
    // the run ~2s after the single transcript write (#5383).
    let mut harness =
        LaneHarness::start_with_heartbeat(&LaneScript::all_passing().with_default(LaneMode::NeverExits), 60, 2);

    harness.wait_for_runs(1);
    let nonces = harness.evidence_nonces();
    for nonce in &nonces {
        harness.write_transcript(nonce, "{}\n");
        harness.touch_heartbeat(nonce);
    }
    let runs = harness.runs_dir();
    let pumped = nonces.clone();
    while_pumping(
        || {
            for nonce in &pumped {
                let dir = runs.join(format!("{nonce}-evidence"));
                let _ = fs::create_dir_all(&dir);
                let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis());
                let _ = fs::write(dir.join("heartbeat"), stamp.to_string());
            }
        },
        || thread::sleep(Duration::from_secs(12)),
    );

    let outstanding = harness.outstanding();
    for nonce in &nonces {
        assert!(
            outstanding.contains(nonce),
            "a lane beating its heartbeat must keep its original nonce, not be cancelled and redispatched; outstanding={outstanding:?} original={nonces:?}"
        );
    }
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
    assert_scratch_checkouts_are_named_for_work(&harness, "a deadline-killed noisy run leaves no checkout of its own");
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

// Every scratch checkout belongs to a session or to a lane slot, and none is
// named for an order.
//
// A worktree per order, accumulating forever, is the leak this catches — and it
// is invisible to any double mounted above the spawn, because there is no
// worktree to leak. What a dispatch registers is its session's tree
// (`sessions/<slug>/tree`, #5425), or the lane slot's own checkout (#4904) when
// the order resolves no session, and either way it is reused by every launch
// that follows. The registered set is therefore bounded by the live session
// count plus the lane ceiling, however many orders run. Anything named after an
// order is the leak, and the shape is what says so.
fn assert_scratch_checkouts_are_named_for_work(harness: &LaneHarness, context: &str) {
    let sessions = fs::canonicalize(harness.runs_dir()).unwrap().join("sessions");
    for checkout in registered_scratch_checkouts(harness) {
        let name = checkout.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_owned();
        let slot = name.strip_prefix("slot-").is_some_and(|index| index.chars().all(|digit| digit.is_ascii_digit()));
        let session = name == "tree" && checkout.parent().and_then(Path::parent) == Some(sessions.as_path());
        assert!(
            slot || session,
            "{context}: {} belongs to neither a session nor a lane slot, so something registered one per order",
            checkout.display(),
        );
    }
}

#[test]
fn the_only_scratch_checkouts_a_bloom_leaves_are_named_for_its_work() {
    // The green path's half of the same invariant: a bloom that runs a construct
    // lane, a verify lane, and a critic against one member registers the slot
    // checkouts those dispatches shared, and never a directory per order.
    let mut harness = LaneHarness::start(&LaneScript::all_passing());

    harness
        .settle("the member resolves", |bloom| bloom.members.first().is_some_and(|member| member.resolution.is_some()));

    assert_scratch_checkouts_are_named_for_work(
        &harness,
        "a resolved member's dispatches shared their slots' checkouts",
    );
}
