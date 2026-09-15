//! A Reconcile lane that declines on work the head does not carry still parks.
//!
//! Two members advance the eager head through separate appends then a third
//! member's append collides and is sent back to Reconcile; its lane declines,
//! and the decline parks naming Reconcile rather than resolving or wedging.
//! The boundary this pins for #5966's resolve-as-current arm: a decline
//! resolves instead of parking only when the member's standing claim is
//! exactly what the head carries, so an uncovered decline must keep its
//! visible park — silently resolving it would drop work the head never held.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::slice::from_ref;

use aether_bloomery::{
    BloomId, CoordinationPolicy, CoordinationState, Digest, Fact, FakeKeyProvider, IntegrationHead, KeyId, MemberPin,
    ResolvedConfigs, Snapshot, StageId, VerificationMode, decode_recorded_decisions, decode_recorded_event,
    signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness, passed};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";
const THIRD: &str = "wp-c";
const SHARED: &str = "crates/example-shared/src/lib.rs";

/// What the first member writes over the seed's `1`.
const LEADING_SHARED: &str = "pub fn shared() -> u8 {\n    11\n}\n";

/// What the third member writes over the same line, so its append onto a head
/// carrying the first member's hunk is a real git collision.
const COLLIDING_SHARED: &str = "pub fn shared() -> u8 {\n    33\n}\n";

/// Eager integration over ordinary standalone member proofs: each member's
/// pass is its own append.
fn eager_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Standalone,
        eager_integration: true,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 0,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("decline-boundary harness")), &[0x0A; 32], *scope_revision);
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

fn claimed(harness: &ScenarioHarness, bloom: BloomId, workpiece: &str) -> bool {
    coordination(harness, bloom).claims.contains_key(workpiece)
}

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

/// Every journaled reconcile dispatch, by workpiece: the Declines lane exits
/// at once, so the outstanding order is gone before a poll can see it — the
/// journaled decision is the stable observation.
fn reconcile_dispatches(harness: &ScenarioHarness, bloom: BloomId) -> Vec<String> {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .iter()
        .filter_map(|row| decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref()).ok())
        .flat_map(|decisions| decisions.effects)
        .filter_map(|effect| match effect {
            aether_bloomery::Decision::DispatchContextualAttempt { dispatch }
                if dispatch.bloom == bloom && dispatch.stage == StageId::Reconcile =>
            {
                Some(dispatch.workpiece.0)
            }
            _ => None,
        })
        .collect()
}

fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

fn worktree(harness: &ScenarioHarness, order: &OutstandingOrder) -> PathBuf {
    harness
        .ledger()
        .iter()
        .find(|run| run.nonce == order.nonce)
        .and_then(|run| run.worktree.as_deref())
        .map(PathBuf::from)
        .expect("the parked local lane recorded its real worktree")
}

fn author(harness: &ScenarioHarness, order: &OutstandingOrder, path: &str, contents: &str) {
    let worktree = worktree(harness, order);
    let generic = worktree.join(CANDIDATE_FILE);
    if generic.is_file() {
        fs::remove_file(&generic).expect("the mock's generic candidate is removed");
    }
    fs::write(worktree.join(path), contents).expect("the parked lane writes its own approved surface");
}

fn parked_author_lane(harness: &mut ScenarioHarness, stage: StageId) -> OutstandingOrder {
    harness.pump_until("a member receives its parked author order", |harness| {
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

fn own_path(workpiece: &str) -> String {
    format!("crates/example-{}/src/eager.rs", workpiece.strip_prefix("wp-").unwrap_or(workpiece))
}

fn seal_trio(harness: &mut ScenarioHarness) -> BloomId {
    let first = harness.author_scope_revision(FIRST, from_ref(&"crates/example-shared/**"));
    let second = harness.author_scope_revision(SECOND, from_ref(&"crates/example-b/**"));
    let third = harness.author_scope_revision(THIRD, from_ref(&"crates/example-shared/**"));
    approve_scopes(harness, &[first, second, third]);
    harness.seal_members(&[(FIRST, first), (SECOND, second), (THIRD, third)])
}

#[test]
fn a_reconcile_decline_on_work_the_head_does_not_carry_still_parks() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing()
        .then_for(FIRST, StageId::Construct, LaneMode::NeverExits)
        .then_for(SECOND, StageId::Construct, LaneMode::NeverExits)
        .then_for(THIRD, StageId::Construct, LaneMode::NeverExits)
        .then_for(THIRD, StageId::Reconcile, LaneMode::Declines);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_policy())
        .script(&script)
        .start("reconcile-decline-without-coverage-parks");
    let bloom = seal_trio(&mut harness);

    // The first member rewrites the shared line and advances the head alone.
    let leading = parked_author_lane(&mut harness, StageId::Construct);
    assert_eq!(leading.workpiece, FIRST, "the seal order admits the first member first");
    author(&harness, &leading, SHARED, LEADING_SHARED);
    harness.release_parked_lanes(from_ref(&leading));
    harness.pump_until("the eager head advances over the first member", |harness| {
        covers(&head_coverage(harness, bloom), FIRST)
    });

    // The second member touches only its own file and appends cleanly.
    let following = parked_author_lane(&mut harness, StageId::Construct);
    assert_eq!(following.workpiece, SECOND);
    author(&harness, &following, &own_path(SECOND), "pub const FOLLOWING: u8 = 2;\n");
    harness.release_parked_lanes(from_ref(&following));
    harness.pump_until("the eager head grows to cover the second member", |harness| {
        let coverage = head_coverage(harness, bloom);
        covers(&coverage, FIRST) && covers(&coverage, SECOND)
    });
    let partial: Vec<MemberPin> = head_coverage(&harness, bloom);
    assert_eq!(advances(&harness, bloom).len(), 2, "one journaled advance per member");

    // The third member rewrites the same shared line the first member took,
    // so its append onto the two-member head is a real git collision.
    let colliding = parked_author_lane(&mut harness, StageId::Construct);
    assert_eq!(colliding.workpiece, THIRD);
    author(&harness, &colliding, SHARED, COLLIDING_SHARED);
    harness.release_parked_lanes(from_ref(&colliding));
    harness.pump_until("the third member's append collides with the two-member head", |harness| {
        !collisions(harness, bloom).is_empty()
    });
    let collided = collisions(&harness, bloom);
    assert_eq!(collided.len(), 1, "one journaled collision: {collided:?}");
    assert_eq!(
        collided[0].iter().map(|pin| pin.workpiece.0.as_str()).collect::<Vec<_>>(),
        vec![THIRD],
        "the collision names only the input that would not place: {collided:?}",
    );

    // Only the uncovered member is sent back; the integrated members keep
    // their claims and never see a lap.
    harness.pump_until("the uncovered member is sent back", |harness| {
        reconcile_dispatches(harness, bloom).iter().any(|workpiece| workpiece == THIRD)
    });
    let dispatched = reconcile_dispatches(&harness, bloom);
    assert_eq!(dispatched, vec![THIRD.to_owned()], "only the uncovered member is re-dispatched: {dispatched:?}");
    assert_eq!(head_coverage(&harness, bloom), partial, "the surviving coverage is retained");
    assert!(claimed(&harness, bloom, FIRST) && claimed(&harness, bloom, SECOND));
    assert!(claimed(&harness, bloom, THIRD), "the collided member verified, so its claim stands unintegrated");

    // Its lane declines, and the decline parks naming Reconcile: the head
    // does not carry the work, so there is nothing to resolve as current.
    harness.pump_until("the declined reconcile parks", |harness| {
        harness.bloom(bloom).members.iter().any(|member| member.workpiece.0 == THIRD && member.park.is_some())
    });
    let view = harness.bloom(bloom);
    let member = view.members.iter().find(|member| member.workpiece.0 == THIRD).expect("the member is listed");
    assert_eq!(member.park.as_ref().map(|park| park.stage), Some(StageId::Reconcile));
    assert!(member.wedge.is_none(), "a park is not a wedge");
    assert_eq!(head_coverage(&harness, bloom), partial, "the park moves no head");
}
