//! Eager integration (ADR-0218) over the real coordinator: the immutable head
//! advances one verified member at a time instead of waiting for the whole
//! bloom, and an append that collides sends only the colliding member back
//! while the head keeps the coverage it already earned.
//!
//! The headline of the eager policy is that a partial head is durable, and
//! nothing below reads test-side bookkeeping for it: the coverage assertions
//! come off the coordinator's own journal, replayed into a [`Snapshot`] the way
//! an operator read would, and the append verdicts are the journaled
//! [`Fact::IntegrationAdvanced`] / [`Fact::IntegrationAppendConflicted`] rows. A
//! reducer that advanced the head only at final readiness, or that ejected both
//! members of a collision, passes every unit test in the coordination module and
//! fails here.

use std::fs;
use std::path::PathBuf;
use std::slice::from_ref;

use aether_bloomery::{
    BloomId, BloomStatus, CoordinationPolicy, CoordinationState, Digest, Fact, FakeKeyProvider, IntegrationHead, KeyId,
    MemberPin, ResolvedConfigs, Snapshot, StageId, Transformation, VerificationMode, decode_recorded_decisions,
    decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness, passed};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";
const SHARED: &str = "crates/example-shared/src/lib.rs";

/// What the first member to finish writes over the seed's `1`.
const LEADING_SHARED: &str = "pub fn shared() -> u8 {\n    11\n}\n";

/// What the second writes over the same line, so the two hunks overlap.
const FOLLOWING_SHARED: &str = "pub fn shared() -> u8 {\n    22\n}\n";

/// Eager integration over ordinary standalone member proofs.
///
/// Standalone rather than contextual verification so each member's pass is its
/// own physical run: the scenario is about the head moving per member, and a
/// composition that verified both at once would hide exactly that.
fn eager_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        verification: VerificationMode::Standalone,
        eager_integration: true,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 0,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("eager harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
    }
}

fn replay_snapshot(store: &mut dyn StoreBackend) -> Snapshot {
    store.replay_journal().expect("the coordinator journal replays").into_iter().fold(
        Snapshot::default(),
        |snapshot, row| {
            let event = decode_recorded_event(&row.event, row.event_schema.as_deref())
                .expect("the harness journal event decodes");
            let decisions = decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
                .expect("the harness journal decisions decode");
            snapshot.apply(&event, &decisions, &ResolvedConfigs::default())
        },
    )
}

/// The coordination state the coordinator's own journal projects for `bloom`.
fn coordination(harness: &ScenarioHarness, bloom: BloomId) -> CoordinationState {
    let snapshot = replay_snapshot(&mut harness.commission_store());
    let record = snapshot.blooms.get(&bloom).expect("the sealed bloom projects");
    *record.coordination.clone().expect("an eager bloom carries coordination state")
}

fn head(harness: &ScenarioHarness, bloom: BloomId) -> IntegrationHead {
    coordination(harness, bloom).integration.head
}

fn head_coverage(harness: &ScenarioHarness, bloom: BloomId) -> Vec<MemberPin> {
    head(harness, bloom).coverage
}

fn covers(coverage: &[MemberPin], workpiece: &str) -> bool {
    coverage.iter().any(|pin| pin.workpiece.0 == workpiece)
}

/// Whether the coordination state holds `workpiece`'s verified resolution claim.
///
/// An eager bloom deliberately files no legacy member claim — the proof lives in
/// the coordination state until the selected root resolves — so the member view's
/// `resolution` reads `None` for a member that has already passed, and asserting
/// on it would assert nothing.
fn claimed(harness: &ScenarioHarness, bloom: BloomId, workpiece: &str) -> bool {
    coordination(harness, bloom).claims.contains_key(workpiece)
}

/// Every journaled fact `select` matches, in journal order.
fn facts<T>(harness: &ScenarioHarness, select: impl Fn(&Fact) -> Option<T>) -> Vec<T> {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .iter()
        .filter_map(|row| decode_recorded_event(&row.event, row.event_schema.as_deref()).ok())
        .filter_map(|event| select(&event.fact))
        .collect()
}

fn advances(harness: &ScenarioHarness, bloom: BloomId) -> Vec<Vec<MemberPin>> {
    facts(harness, |fact| match fact {
        Fact::IntegrationAdvanced { bloom: advanced, head, .. } if *advanced == bloom => Some(head.coverage.clone()),
        _ => None,
    })
}

fn collisions(harness: &ScenarioHarness, bloom: BloomId) -> Vec<Vec<MemberPin>> {
    facts(harness, |fact| match fact {
        Fact::IntegrationAppendConflicted { bloom: collided, input, .. } if *collided == bloom => {
            Some(input.members.clone())
        }
        _ => None,
    })
}

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn sibling_of(workpiece: &str) -> &'static str {
    if workpiece == FIRST {
        SECOND
    } else {
        FIRST
    }
}

/// The real scratch worktree the coordinator spawned `order`'s child in.
fn worktree(harness: &ScenarioHarness, order: &OutstandingOrder) -> PathBuf {
    harness
        .ledger()
        .iter()
        .find(|run| run.nonce == order.nonce)
        .and_then(|run| run.worktree.as_deref())
        .map(PathBuf::from)
        .expect("the parked local lane recorded its real worktree")
}

/// Replace the mock's generic candidate with one this member's surface covers.
///
/// The mock writes one nonce-stamped file for every lane, so two members left to
/// themselves always collide on it. Authoring the candidate by hand is how a
/// scenario says which members touch the same lines and which do not.
fn author(harness: &ScenarioHarness, order: &OutstandingOrder, path: &str, contents: &str) {
    let worktree = worktree(harness, order);
    let generic = worktree.join(CANDIDATE_FILE);
    if generic.is_file() {
        fs::remove_file(&generic).expect("the mock's generic candidate is removed");
    }
    fs::write(worktree.join(path), contents).expect("the parked lane writes its own approved surface");
}

/// Wait for the next parked author lane, standing in its real worktree.
///
/// One at a time, and not because the scenario wants it that way: eager
/// construction admission submits an author order only into an idle executor, so
/// a parked child holds the slot and its sibling's order is not dispatched until
/// this one exits.
fn parked_author_lane(harness: &mut ScenarioHarness, stage: StageId) -> OutstandingOrder {
    harness.pump_until("a member receives its parked author order", |harness| {
        // The seal answers an outstanding `verify.base` on a five-second budget
        // and gives up silently. On a loaded host that order arrives later, and
        // a bloom whose base is unproven dispatches no author lane at all.
        if let Some(base) = harness.orders().into_iter().find(|order| stage_of(order) == StageId::BaseVerify) {
            harness.upload_admitted(&passed(&base));
            return false;
        }
        harness.orders().iter().any(|order| stage_of(order) == stage)
    });
    let order = harness
        .orders()
        .into_iter()
        .find(|order| stage_of(order) == stage)
        .expect("pump_until saw the parked author order");

    harness.pump_until("the parked author child reached its real worktree", |harness| {
        harness.ledger().iter().any(|run| run.nonce == order.nonce)
    });
    order
}

/// Seal A and B over the formatted example project under the eager policy.
fn sealed_pair(harness: &mut ScenarioHarness, surfaces: [&str; 2]) -> BloomId {
    let first = harness.author_scope_revision(FIRST, from_ref(&surfaces[0]));
    let second = harness.author_scope_revision(SECOND, from_ref(&surfaces[1]));
    approve_scopes(harness, &[first, second]);
    harness.seal_members(&[(FIRST, first), (SECOND, second)])
}

/// The file each member of the independent pair owns inside its own surface.
fn own_path(workpiece: &str) -> String {
    format!("crates/example-{}/src/eager.rs", workpiece.strip_prefix("wp-").unwrap_or(workpiece))
}

#[test]
fn an_eager_head_advances_per_member_rather_than_waiting_for_the_bloom() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then_for(FIRST, StageId::Construct, LaneMode::NeverExits).then_for(
        SECOND,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_policy())
        .script(&script)
        .start("eager-head-advances-per-member");
    let bloom = sealed_pair(&mut harness, ["crates/example-a/**", "crates/example-b/**"]);

    let leading = parked_author_lane(&mut harness, StageId::Construct);
    let lagging = sibling_of(&leading.workpiece).to_owned();
    author(&harness, &leading, &own_path(&leading.workpiece), "pub const LEADING: u8 = 1;\n");
    harness.release_parked_lanes(from_ref(&leading));

    harness.pump_until("the eager head advances over the member that finished first", |harness| {
        covers(&head_coverage(harness, bloom), &leading.workpiece)
    });
    let partial = head_coverage(&harness, bloom);
    assert_eq!(partial.len(), 1, "the partial head carries exactly the member that has been verified: {partial:?}");
    assert!(!covers(&partial, &lagging), "{lagging} has not finished, so its pin cannot be on the head: {partial:?}");
    assert!(!claimed(&harness, bloom, &lagging), "the head moved while {lagging} was still unverified");
    assert_eq!(
        advances(&harness, bloom).len(),
        1,
        "the partial advance is journaled on its own rather than folded into the final resolve",
    );

    // Now the second member finishes and the same head grows to cover both.
    let following = parked_author_lane(&mut harness, StageId::Construct);
    assert_eq!(following.workpiece, lagging, "the sibling's author order follows its parked predecessor");
    author(&harness, &following, &own_path(&following.workpiece), "pub const FOLLOWING: u8 = 2;\n");
    harness.release_parked_lanes(from_ref(&following));

    harness.pump_until("the eager head grows to cover both members", |harness| {
        let coverage = head_coverage(harness, bloom);
        covers(&coverage, FIRST) && covers(&coverage, SECOND)
    });
    let advanced = advances(&harness, bloom);
    assert_eq!(advanced.len(), 2, "one journaled advance per member: {advanced:?}");
    assert_eq!(advanced[0], partial, "the first advance is the partial head, retained exactly");

    harness.pump_until("the bloom resolves the root its own head selected", |harness| {
        replay_snapshot(&mut harness.commission_store())
            .blooms
            .get(&bloom)
            .is_some_and(|record| record.resolved_head.is_some())
    });
    let snapshot = replay_snapshot(&mut harness.commission_store());
    let record = snapshot.blooms.get(&bloom).expect("the eager bloom remains projected");
    let state = record.coordination.as_ref().expect("the eager coordination state remains projected");
    assert_eq!(
        record.resolved_head,
        Some(state.integration.head.candidate.checkout),
        "final resolution adopts the selected root rather than re-folding the leaves",
    );
    assert_eq!(record.resolved_tree, Some(state.integration.head.candidate.tree));
    assert_eq!(state.integration.head.coverage.len(), 2, "the resolved root covers every active member");
    assert_eq!(
        harness.ledger().iter().filter(|run| run.stage == Some(StageId::AggregateVerify)).count(),
        1,
        "the selected root is verified once, not once per partial advance",
    );

    harness.await_landing(bloom, BloomStatus::Landed);
}

#[test]
fn an_eager_append_collision_sends_back_only_the_collider() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FIRST, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Construct, LaneMode::NeverExits)
        .then_for(FIRST, StageId::Reconcile, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Reconcile, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_policy())
        .script(&script)
        .start("eager-append-collision");
    let bloom = sealed_pair(&mut harness, ["crates/example-shared/**", "crates/example-shared/**"]);

    // Both members rewrite the same line of the same file, so the second one's
    // append onto the head the first advanced is a real git collision.
    let leading = parked_author_lane(&mut harness, StageId::Construct);
    let colliding = sibling_of(&leading.workpiece).to_owned();
    author(&harness, &leading, SHARED, LEADING_SHARED);
    harness.release_parked_lanes(from_ref(&leading));
    harness.pump_until("the eager head advances over the member that finished first", |harness| {
        covers(&head_coverage(harness, bloom), &leading.workpiece)
    });
    let survivor_head = head(&harness, bloom);
    let partial = survivor_head.coverage.clone();

    let following = parked_author_lane(&mut harness, StageId::Construct);
    assert_eq!(following.workpiece, colliding, "the sibling's author order follows its parked predecessor");
    author(&harness, &following, SHARED, FOLLOWING_SHARED);
    harness.release_parked_lanes(from_ref(&following));

    harness.pump_until("the second member's append collides with the head the first advanced", |harness| {
        !collisions(harness, bloom).is_empty()
    });
    let collided = collisions(&harness, bloom);
    assert_eq!(collided.len(), 1, "one journaled collision: {collided:?}");
    assert_eq!(
        collided[0].iter().map(|pin| pin.workpiece.0.as_str()).collect::<Vec<_>>(),
        vec![colliding.as_str()],
        "the collision names only the input that would not place, not the head it collided with",
    );
    assert_eq!(
        head_coverage(&harness, bloom),
        partial,
        "the surviving coverage is retained: a collision ejects the collider, not the bloom",
    );

    harness.pump_until("only the collider is sent back", |harness| {
        harness.orders().iter().any(|order| order.workpiece == colliding && stage_of(order) == StageId::Reconcile)
    });
    let orders = harness.orders();
    assert!(
        orders.iter().all(|order| order.workpiece != leading.workpiece),
        "the member already on the head is not re-dispatched: {orders:?}",
    );
    assert!(
        claimed(&harness, bloom, &leading.workpiece),
        "the surviving member keeps the verified claim its sibling's collision did not touch",
    );

    // The ejected member's lap is seeded from the candidate its own lane
    // produced, stands on the head it collided with, and carries the collision
    // diagnostic.
    let reconcile = parked_author_lane(&mut harness, StageId::Reconcile);
    assert_eq!(reconcile.workpiece, colliding);
    let transformation: Transformation =
        from_bytes(&reconcile.transformation).expect("a recorded order carries a Transformation");
    assert_eq!(
        transformation.inputs[0], collided[0][0].candidate.tree,
        "the lap is seeded from the candidate the ejected member's own lane produced",
    );
    assert_eq!(
        transformation.checkout, survivor_head.candidate.checkout,
        "the lap's working tree is the folded head, not the candidate that would not place",
    );
    assert!(
        transformation.description.is_some_and(|order| order.contains(SHARED)),
        "the lap is handed the conflicting path rather than being asked to guess it",
    );

    author(&harness, &reconcile, SHARED, "pub fn shared() -> u8 {\n    33\n}\n");
    harness.release_parked_lanes(from_ref(&reconcile));

    harness.pump_until("the reconciled candidate is prepared and re-queued for verification", |harness| {
        let state = coordination(harness, bloom);
        state.prepared.contains_key(colliding.as_str())
            && state.requests.iter().any(|request| request.member.workpiece.0 == colliding)
    });
    assert!(
        claimed(&harness, bloom, &leading.workpiece),
        "the surviving member's verified claim outlives its sibling's reconcile lap",
    );
    let prepared = coordination(&harness, bloom).prepared.remove(colliding.as_str()).expect("the prepared candidate");
    assert_eq!(
        prepared.diff_base, survivor_head.candidate,
        "preparation merges the repair onto the advanced head, never back onto the generation base",
    );
    assert_eq!(prepared.context.starting_head, survivor_head, "the repair is pinned to the head that survived");

    // The prepared candidate already carries the survivor's tree, so its append
    // places on the very head the collision refused.
    harness.pump_until("the repaired contribution joins the head the survivor already holds", |harness| {
        let coverage = head_coverage(harness, bloom);
        covers(&coverage, FIRST) && covers(&coverage, SECOND)
    });
    let advanced = advances(&harness, bloom);
    assert_eq!(advanced.len(), 2, "the repaired member appends onto the partial head: {advanced:?}");
    assert_eq!(advanced[0], partial, "the survivor's advance is retained exactly, not rebuilt");
    assert_eq!(
        collisions(&harness, bloom).len(),
        1,
        "the repair places rather than colliding again on the head it was prepared against",
    );

    harness.pump_until("the bloom resolves the root the repaired head selected", |harness| {
        replay_snapshot(&mut harness.commission_store())
            .blooms
            .get(&bloom)
            .is_some_and(|record| record.resolved_head.is_some())
    });
    let snapshot = replay_snapshot(&mut harness.commission_store());
    let record = snapshot.blooms.get(&bloom).expect("the eager bloom remains projected");
    let state = record.coordination.as_ref().expect("the eager coordination state remains projected");
    assert_eq!(
        record.resolved_head,
        Some(state.integration.head.candidate.checkout),
        "resolution adopts the head the repair rejoined rather than re-folding the leaves",
    );
    assert_eq!(
        harness.ledger().iter().filter(|run| run.stage == Some(StageId::AggregateVerify)).count(),
        1,
        "the root the collision-free head selected is verified once",
    );

    harness.await_landing(bloom, BloomStatus::Landed);
}
