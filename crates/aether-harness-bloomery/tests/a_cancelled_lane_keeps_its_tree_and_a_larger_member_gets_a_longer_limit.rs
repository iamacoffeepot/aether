//! A dispatched lane runs under a sealed execution limit (ADR-0177). Two things
//! about that limit cross the whole coordinator, so both are proved here rather
//! than at a unit seam.
//!
//! When the limit passes, the run is cancelled where it stands. Until #5998 the
//! tree it had built was discarded with it: the cancel released the lane slot,
//! the next dispatch reset that checkout, and the retry lap re-derived an hour's
//! work from a resumed context or from nothing. Now the cancel captures the
//! tree first, and it lands in the same member-checkpoint ref a *failing*
//! construct's tree lands in — so the retry lap resumes from it.
//!
//! And the limit is no longer one number for every member. The band a stage
//! dispatches at is resolved from the size the member's sealed scope revision
//! routed it into, so a size L member gets longer than a size S one out of the
//! same authored catalog.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{Digest, FakeKeyProvider, KeyId, StageId, signed_approval};
use aether_bloomery_github::member_checkpoint_ref_name;
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::CommissionBackend;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

/// The member whose construct lane is cancelled at its limit, and the size S
/// half of the banding pair.
const SMALL: &str = "wp-a";

/// The size L half of the banding pair.
const LARGE: &str = "wp-b";

/// What the banding scenario seals as each stage's authored wall clock, in
/// seconds. Long enough that neither member is cancelled while the scenario
/// reads its order, and round enough that the bands it resolves — half of it at
/// size S, twice it at size L — are unmistakable in a failure message.
const AUTHORED_WALL_CLOCK_SECS: u64 = 600;

fn approve(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("execution-limit harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the authored scope retains its signed approval");
    }
}

/// The commit a ref points at, or `None` when the authority holds no such ref.
///
/// `for-each-ref` rather than `rev-parse`, because the scenario waits for the
/// ref to appear and `rev-parse` of an absent ref is a failed git command, not
/// an empty answer.
fn ref_commit(authority: &Repo, name: &str) -> Option<String> {
    let listed = authority.git(&["for-each-ref", "--format=%(objectname)", name]);
    listed.split_whitespace().next().map(str::to_owned)
}

#[test]
fn a_lane_cancelled_at_its_sealed_limit_is_captured_to_the_member_checkpoint() {
    let authority = Repo::with_formatted_example_project();
    // The mock parks after writing its candidate, so only the sealed deadline
    // can end this run — and it ends it with a dirty tree, which is the whole
    // situation. The retry occurrence falls back to the passing default.
    let script = LaneScript::all_passing().then_for(SMALL, StageId::Construct, LaneMode::NeverExits);
    let mut harness =
        HarnessBuilder::local_authority(&authority).script(&script).wall_clock_secs(8).start("cancelled-lane-capture");
    let scope = harness.author_sized_scope_revision(SMALL, &["crates/example-a/**"], "M");
    approve(&harness, &[scope]);
    let bloom = harness.seal_member(SMALL, scope);

    let checkpoint_ref = member_checkpoint_ref_name(&bloom, SMALL);
    harness.pump_until("the cancelled construct's tree reaches the member-checkpoint ref", |_| {
        ref_commit(&authority, &checkpoint_ref).is_some()
    });

    let checkpoint = ref_commit(&authority, &checkpoint_ref).expect("the checkpoint ref resolves");
    assert_eq!(
        authority.git(&["cat-file", "-t", &checkpoint]).trim(),
        "commit",
        "the capture is a commit, the same shape a failing construct's capture is",
    );
    let names = authority.git(&["ls-tree", "-r", "--name-only", &checkpoint]);
    assert!(
        names.lines().any(|name| name == CANDIDATE_FILE),
        "the checkpoint carries what the cancelled lane had written, not an empty tree: {names}",
    );

    // The point of keeping the tree: the lap that follows starts from it. The
    // executor renders a selected checkpoint as `--seeded <commit>`, which is
    // what grows the prompt's `## Seeded checkpoint` section.
    harness.pump_until("the member re-dispatches its construct from the checkpoint", |harness| {
        harness.ledger().iter().any(|run| run.argv.iter().any(|arg| arg == "--seeded"))
    });
    let seeded = harness
        .ledger()
        .into_iter()
        .find(|run| run.argv.iter().any(|arg| arg == "--seeded"))
        .expect("the retry lap carries a checkpoint");
    assert_eq!(seeded.stage, Some(StageId::Construct), "only the construct line resumes from a checkpoint");
    assert!(
        seeded.argv.windows(2).any(|pair| pair[0] == "--seeded" && pair[1] == checkpoint),
        "the retry names the very checkpoint the cancel captured ({checkpoint}): {:?}",
        seeded.argv,
    );
}

#[test]
fn a_size_l_member_resolves_a_longer_limit_than_a_size_s_one() {
    let authority = Repo::with_formatted_example_project();
    // Both constructs park, so both orders stay outstanding with the deadlines
    // their dispatches minted — which is the pair this scenario compares.
    let script = LaneScript::all_passing().then_for(SMALL, StageId::Construct, LaneMode::NeverExits).then_for(
        LARGE,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .script(&script)
        .wall_clock_secs(AUTHORED_WALL_CLOCK_SECS)
        .start("sized-execution-limits");
    let small = harness.author_sized_scope_revision(SMALL, &["crates/example-a/**"], "S");
    let large = harness.author_sized_scope_revision(LARGE, &["crates/example-b/**"], "L");
    approve(&harness, &[small, large]);
    harness.seal_members(&[(SMALL, small), (LARGE, large)]);

    harness.pump_until("both members hold a construct order", |harness| harness.orders().len() == 2);

    let orders = harness.orders();
    let deadline_of = |workpiece: &str| {
        orders
            .iter()
            .find(|order| order.workpiece == workpiece)
            .unwrap_or_else(|| panic!("{workpiece} holds an outstanding order: {orders:?}"))
            .deadline_unix_millis
    };

    // The two dispatches are minted within a tick of each other out of one
    // authored number, so the whole difference between their deadlines is the
    // band each member's sealed size resolved: half the authored limit at S,
    // twice it at L. The assertion leaves the full authored limit as slack, so
    // it is a statement about the bands rather than about dispatch timing.
    let separation = deadline_of(LARGE).saturating_sub(deadline_of(SMALL));
    assert!(
        separation > AUTHORED_WALL_CLOCK_SECS * 1_000,
        "a size L member must run materially longer than a size S one out of the same catalog; \
         separation was {separation}ms over an authored {AUTHORED_WALL_CLOCK_SECS}s",
    );
}
