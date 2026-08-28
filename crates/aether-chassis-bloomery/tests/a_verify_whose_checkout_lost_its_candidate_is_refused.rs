#![cfg(all(unix, feature = "github"))]

//! A member `Verify` whose checkout no longer carries the candidate the order
//! binds is refused before a lane runs, and the refusal is recorded as
//! machinery rather than as a verdict about the work (ADR-0152, ADR-0195).
//!
//! The two digests a dispatch aims at are independent axes: `checkout` names a
//! git commit, and the evidence subject names the candidate tree. Nothing
//! downstream re-joins them — the reducer admits a verdict that binds the
//! subject the *order* displayed, whatever tree the lane actually stood in — so
//! a checkout that drifted off the candidate produces a verdict about content
//! nobody asked to be judged, filed as though it were about the member's newest
//! work.
//!
//! That happened. On 2026-08-26 the final refine lap of `retention-archive-tier`
//! fixed its failing scenario, and the verify that followed checked out a splice
//! built from the *previous* lap's candidate — a tree carrying the base version
//! of the test, byte-identical to the fold parent. The lap's fix was never
//! judged, the same two verifiers failed again over the same effective content,
//! and the repeated-verifiers ceiling wedged the member for work it had already
//! done. An operator hand-repair was needed to land the bloom.
//!
//! The drift enters here through the operator repair door, which is the one
//! place a `(tree, checkout)` pair reaches the line from outside the executor
//! that minted it: the pair names the member's real candidate tree and a
//! checkout resolving to the sealed base, which is exactly the state the
//! coordinator was in. Which producer left it that way is not what this
//! scenario is about; that the machinery notices before a lane is paid for is.
//!
//! Pre-fix, the repaired member dispatches a `verify.member` lane into the base
//! tree and settles on whatever that gate says about content it never wrote.
//! Post-fix the lane never starts, the member takes machinery rolls instead,
//! and the wedge names the machinery.

use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::testing::digest;
use aether_bloomery::{BackendObjectId, BloomId, CandidateRef, Correspondence, Outcome, decode_hex};
use aether_bloomery::{VERIFY_MEMBER_COMMAND, VerifyFailureSet, WedgeCause, WorkpieceId};
use aether_chassis_bloomery::bloomery::capture_commit_digest;
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::SqliteCorrespondence;
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, OperatorMove, Repo, ScenarioHarness};

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

#[test]
fn a_verify_whose_checkout_lost_its_candidate_is_refused_rather_than_judged() {
    let authority = Repo::bare_authority();
    let roots = HarnessRoots::create();
    let mut harness = HarnessBuilder::local_authority(&authority)
        .roots(&roots)
        .script(&failing_verify())
        .cas_land(false)
        .start("stale-candidate-checkout");
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    // The scripted gate fails every lap, so the member spends its repair
    // ceiling and stops — holding the candidate its last lap captured, and
    // repairable, which is the door the drift arrives through.
    pump_until(&mut harness, "the member spends its repair ceiling", |harness| {
        harness.bloom(bloom).members[0].wedge.is_some()
    });
    let candidate = candidate(&mut harness, bloom);

    // The drift: the member's own tree, paired with a checkout that resolves to
    // the sealed base's *tree* — a bare tree, which is what a spliced dependency
    // claim resolves to (ADR-0196) and what the checkout verbs wrap on the way
    // in. Both digests are real addresses of real objects, and the checkout is
    // minted through the same domain-tagged function the executor mints one
    // with, so nothing downstream can tell this pair from one a stale splice
    // produced. The tree rather than the commit because the correspondence is
    // unique on the backend object: pointing a second digest at the base commit
    // would retire the base's own row and take the whole line down with it.
    let spliced = object(&authority.git(&["rev-parse", "refs/heads/main^{tree}"]));
    let drifted = capture_commit_digest(&spliced);
    SqliteCorrespondence::open(&roots.store_path())
        .expect("the correspondence store opens beside the journal")
        .record(&drifted, &spliced)
        .expect("the drifted checkout records");

    let judged_before = lanes_run(&harness, VERIFY_MEMBER_COMMAND);
    let repair = OperatorMove::Repair {
        at_tick: 0,
        workpiece: WorkpieceId(WORKPIECE.to_owned()),
        candidate: CandidateRef { tree: candidate.tree, checkout: drifted },
        reason: "hand the member back its own tree on a checkout that no longer derives from it".to_owned(),
        operator: "scenario".to_owned(),
    };
    assert!(
        matches!(harness.apply_operator(bloom, &repair), Outcome::OperatorRepairAccepted { .. }),
        "the repair door admits the pair; what the Verify it dispatches then does is the scenario",
    );

    // Settled either way, because the two coordinators settle differently: the
    // fixed one refuses the dispatch until the machinery series is spent and
    // stops, while the unfixed one runs the gate in the base tree and takes
    // whatever verdict it renders there — on this script, a pass, and a member
    // that resolves on a verdict about content it never wrote.
    pump_until(&mut harness, "the repaired member settles", |harness| {
        let member = &harness.bloom(bloom).members[0];
        member.wedge.is_some() || member.resolution.is_some()
    });

    assert_eq!(
        lanes_run(&harness, VERIFY_MEMBER_COMMAND),
        judged_before,
        "no gate may stand in a tree that is not the candidate the order binds",
    );

    let member = harness.bloom(bloom).members[0].clone();
    assert_eq!(
        member.wedge_cause,
        Some(WedgeCause::Machinery),
        "a checkout the host got wrong is a sick host, not rejected work",
    );
    assert_eq!(
        member.wedge.expect("the stopped member records why").repeated_verifiers,
        VerifyFailureSet::EMPTY,
        "a refused dispatch rendered no verdict, so it names no repeated verifier",
    );
    assert!(member.machinery_rolls > 0, "the refusals are counted as the machinery series they are");
}

/// Drive every reactor until `ready`, on a budget a loaded host can meet.
///
/// Not the harness's own `pump_until`, which this cell bounds at thirty
/// seconds. Thirty is a fine bound for a scripted cell and a thin one for this
/// one: every lap here forks a real git worktree and a real lane process, and a
/// machine running the rest of the suite alongside takes longer than that to
/// walk a member to its ceiling.
fn pump_until(harness: &mut ScenarioHarness, what: &str, ready: impl Fn(&mut ScenarioHarness) -> bool) {
    let deadline = Instant::now() + Duration::from_mins(3);
    while !ready(harness) {
        assert!(Instant::now() < deadline, "{what} did not happen inside the scenario's budget");
        harness.tick();
        thread::sleep(Duration::from_millis(25));
    }
}

/// The candidate the member's cursor holds.
fn candidate(harness: &mut ScenarioHarness, bloom: BloomId) -> CandidateRef {
    harness.bloom(bloom).members[0]
        .cursor
        .as_ref()
        .and_then(|cursor| cursor.candidate)
        .expect("a member that reached its Verify ceiling captured a candidate")
}

/// How many times the mock lane has been dispatched under `command`.
fn lanes_run(harness: &ScenarioHarness, command: &str) -> usize {
    harness.ledger().iter().filter(|run| run.command == command).count()
}

/// A git sha as the opaque backend object the correspondence stores.
fn object(sha: &str) -> BackendObjectId {
    BackendObjectId::new(decode_hex(sha.trim()).expect("git printed a hex sha"))
}

/// Every lap passes except the mechanical gate, which fails often enough to
/// spend the member's whole repair budget and stop it.
fn failing_verify() -> LaneScript {
    (0..4).fold(LaneScript::all_passing(), |script, _| script.then(VERIFY_MEMBER_COMMAND, LaneMode::Fail))
}
