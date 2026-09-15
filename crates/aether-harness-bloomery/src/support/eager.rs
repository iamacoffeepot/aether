//! Reads and lane-side edits an eager-integration scenario needs (ADR-0218).
//!
//! Every assertion an eager scenario makes about the product comes off the
//! coordinator's own journal, replayed into a [`Snapshot`] the way an operator
//! read would, rather than off test-side bookkeeping. These helpers are that
//! read path, plus the two lane-side edits a scenario needs to say which members
//! touch the same lines: authoring a parked lane's candidate by hand, and
//! waiting for a parked lane to reach its real worktree.
//!
//! They live here rather than in one scenario file because more than one
//! scenario asks the same questions of the same journal, and a second copy of a
//! journal reader is a second thing to keep true.

use std::path::PathBuf;
use std::{fs, slice};

use aether_bloomery::{
    BloomId, CoordinationState, Digest, Fact, FakeKeyProvider, IntegrationHead, KeyId, MemberPin, ResolvedConfigs,
    Snapshot, StageId, decode_recorded_decisions, decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::CANDIDATE_FILE;
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;

use crate::harness::ScenarioHarness;
use crate::harness::drive::passed;

/// Sign a member scope revision so the seal admits it.
///
/// # Panics
/// The commission store refuses the signed approval.
pub fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("eager harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
    }
}

/// Project the coordinator's journal the way an operator read would.
///
/// # Panics
/// The journal does not replay or a recorded row does not decode.
pub fn replay_snapshot(store: &mut dyn StoreBackend) -> Snapshot {
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
///
/// # Panics
/// The bloom is not projected or carries no coordination state.
#[must_use]
pub fn coordination(harness: &ScenarioHarness, bloom: BloomId) -> CoordinationState {
    let snapshot = replay_snapshot(&mut harness.commission_store());
    let record = snapshot.blooms.get(&bloom).expect("the sealed bloom projects");
    *record.coordination.clone().expect("an eager bloom carries coordination state")
}

/// The assembled product tree, which the journal still spells `head`.
#[must_use]
pub fn product(harness: &ScenarioHarness, bloom: BloomId) -> IntegrationHead {
    coordination(harness, bloom).integration.head
}

/// Which members the assembled product carries.
#[must_use]
pub fn product_coverage(harness: &ScenarioHarness, bloom: BloomId) -> Vec<MemberPin> {
    product(harness, bloom).coverage
}

#[must_use]
pub fn covers(coverage: &[MemberPin], workpiece: &str) -> bool {
    coverage.iter().any(|pin| pin.workpiece.0 == workpiece)
}

/// Whether the coordination state holds `workpiece`'s verified resolution claim.
///
/// An eager bloom deliberately files no legacy member claim — the proof lives in
/// the coordination state until the selected root resolves — so the member
/// view's `resolution` reads `None` for a member that has already passed, and
/// asserting on it would assert nothing.
#[must_use]
pub fn claimed(harness: &ScenarioHarness, bloom: BloomId, workpiece: &str) -> bool {
    coordination(harness, bloom).claims.contains_key(workpiece)
}

/// Every journaled fact `select` matches, in journal order.
///
/// # Panics
/// The journal does not replay.
pub fn facts<T>(harness: &ScenarioHarness, select: impl Fn(&Fact) -> Option<T>) -> Vec<T> {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .iter()
        .filter_map(|row| decode_recorded_event(&row.event, row.event_schema.as_deref()).ok())
        .filter_map(|event| select(&event.fact))
        .collect()
}

/// The coverage each journaled fold produced, oldest first.
#[must_use]
pub fn folds(harness: &ScenarioHarness, bloom: BloomId) -> Vec<Vec<MemberPin>> {
    facts(harness, |fact| match fact {
        Fact::IntegrationAdvanced { bloom: advanced, head, .. } if *advanced == bloom => Some(head.coverage.clone()),
        _ => None,
    })
}

/// The members each journaled fold collision named, oldest first.
#[must_use]
pub fn collisions(harness: &ScenarioHarness, bloom: BloomId) -> Vec<Vec<MemberPin>> {
    facts(harness, |fact| match fact {
        Fact::IntegrationAppendConflicted { bloom: collided, input, .. } if *collided == bloom => {
            Some(input.members.clone())
        }
        _ => None,
    })
}

/// # Panics
/// The recorded order does not carry a decodable stage.
#[must_use]
pub fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

/// The real scratch worktree the coordinator spawned `order`'s child in.
///
/// # Panics
/// No parked lane run recorded a worktree for this order.
#[must_use]
pub fn worktree(harness: &ScenarioHarness, order: &OutstandingOrder) -> PathBuf {
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
///
/// # Panics
/// The parked lane's worktree is not writable.
pub fn author(harness: &ScenarioHarness, order: &OutstandingOrder, path: &str, contents: &str) {
    let worktree = worktree(harness, order);
    let generic = worktree.join(CANDIDATE_FILE);
    if generic.is_file() {
        fs::remove_file(&generic).expect("the mock's generic candidate is removed");
    }
    fs::write(worktree.join(path), contents).expect("the parked lane writes its own approved surface");
}

/// Wait for a parked lane at `stage` for `workpiece`, standing in its real
/// worktree.
///
/// The bloom's outstanding `verify.base` is answered along the way: the seal
/// answers it on a five-second budget and gives up silently, and on a loaded
/// host that order arrives later — a bloom whose base is unproven dispatches no
/// author lane at all.
///
/// # Panics
/// The parked order never arrives inside the harness's pump budget.
pub fn parked_lane(harness: &mut ScenarioHarness, workpiece: &str, stage: StageId) -> OutstandingOrder {
    parked_lane_matching(harness, stage, |order| order.workpiece == workpiece)
}

/// Wait for whichever member reaches `stage` first, standing in its real
/// worktree. The same base-verify answering as [`parked_lane`].
///
/// # Panics
/// No parked order at `stage` arrives inside the harness's pump budget.
pub fn parked_lane_at(harness: &mut ScenarioHarness, stage: StageId) -> OutstandingOrder {
    parked_lane_matching(harness, stage, |_| true)
}

fn parked_lane_matching(
    harness: &mut ScenarioHarness,
    stage: StageId,
    select: impl Fn(&OutstandingOrder) -> bool,
) -> OutstandingOrder {
    let matches = |order: &OutstandingOrder| stage_of(order) == stage && select(order);
    harness.pump_until("a member receives its parked order", |harness| {
        if let Some(base) = harness.orders().into_iter().find(|order| stage_of(order) == StageId::BaseVerify) {
            harness.upload_admitted(&passed(&base));
            return false;
        }
        harness.orders().iter().any(matches)
    });
    let order = harness.orders().into_iter().find(matches).expect("pump_until saw the parked order");

    harness.pump_until("the parked child reached its real worktree", |harness| {
        harness.ledger().iter().any(|run| run.nonce == order.nonce)
    });
    order
}

/// Author a parked lane's candidate and let it exit.
pub fn author_and_release(harness: &mut ScenarioHarness, order: &OutstandingOrder, path: &str, contents: &str) {
    author(harness, order, path, contents);
    harness.release_parked_lanes(slice::from_ref(order));
}
