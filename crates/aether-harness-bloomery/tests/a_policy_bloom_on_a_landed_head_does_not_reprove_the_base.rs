//! A contextual policy bloom that lands must file the base receipt its own
//! landing stands on, so the next policy seal does not re-prove the base.
//!
//! Pre-fix (#5962): a contextual landing resolved through shared `PassedIn`
//! receipts, which never entered the aggregate-verify memo, so
//! `landed_base_receipt` found no proof, filed no `RecordBaseReceipt`, and the
//! next seal dispatched a bloom-less `BaseVerify` while constructs waited.
//! The seal-side acceptance (#5940) is the other half: a coordinated seal
//! accepts a landed receipt whose tree is the fold tree.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    BloomId, BloomStatus, CoordinationPolicy, Decision, Digest, Fact, FakeKeyProvider, KeyId, ResolvedConfigs,
    Snapshot, StageId, VerificationMode, decode_recorded_decisions, decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";
const NEXT: &str = "wp-c";

fn contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: false,
        max_run_members: 2,
        max_serial_requests: 2,
        max_attribution_probes: 2,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("policy-landing harness")), &[0x0A; 32], *scope_revision);
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

fn seal_effects(harness: &ScenarioHarness) -> Vec<Vec<Decision>> {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .into_iter()
        .filter_map(|row| {
            let event = decode_recorded_event(&row.event, row.event_schema.as_deref()).ok()?;
            matches!(event.fact, Fact::Seal(_)).then(|| {
                decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
                    .expect("the seal's decisions decode")
                    .effects
            })
        })
        .collect()
}

fn land_effects(harness: &ScenarioHarness, bloom: BloomId) -> Vec<Decision> {
    harness
        .commission_store()
        .replay_journal()
        .expect("the coordinator journal replays")
        .into_iter()
        .filter_map(|row| {
            let event = decode_recorded_event(&row.event, row.event_schema.as_deref()).ok()?;
            match &event.fact {
                Fact::Land { bloom: landed, .. } if *landed == bloom => Some(
                    decode_recorded_decisions(&row.decisions, row.decisions_schema_digest.as_deref())
                        .expect("the land's decisions decode")
                        .effects,
                ),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .into_iter()
        .last()
        .expect("the landing journals its decisions")
}

fn dispatched_base_verify(effects: &[Decision]) -> bool {
    effects.iter().any(|effect| matches!(effect, Decision::DispatchBaseVerify { .. }))
}

fn queued_construction(effects: &[Decision]) -> bool {
    effects.iter().any(|effect| matches!(effect, Decision::QueueConstructionAdmission { .. }))
}

fn complete_joint_constructions(harness: &mut ScenarioHarness) {
    harness.hold_member_verification(true);
    harness.pump_until("both members receive their parked Construct orders", |harness| {
        let orders = harness.orders();
        assert!(orders.len() <= 2, "only the two sealed members can be dispatched: {orders:?}");
        orders.len() == 2
    });
    let constructs = harness.orders();
    assert!(
        constructs
            .iter()
            .all(|order| from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::Construct)),
        "the barrier holds exactly the two Construct orders: {constructs:?}"
    );

    harness.pump_until("both parked Construct children reached their real worktrees", |harness| {
        let runs = harness.ledger();
        constructs.iter().all(|order| runs.iter().any(|run| run.nonce == order.nonce))
    });
    let runs = harness.ledger();
    for (workpiece, path) in
        [(FIRST, "crates/example-a/src/contextual.rs"), (SECOND, "crates/example-b/src/contextual.rs")]
    {
        let order = constructs
            .iter()
            .find(|order| order.workpiece == workpiece)
            .expect("the jointly admitted member has a Construct order");
        let worktree = runs
            .iter()
            .find(|run| run.nonce == order.nonce)
            .and_then(|run| run.worktree.as_deref())
            .map(Path::new)
            .expect("the parked local Construct recorded its real worktree");
        fs::remove_file(worktree.join(CANDIDATE_FILE)).expect("the mock's generic candidate is removed");
        fs::write(worktree.join(path), format!("pub const MEMBER: &str = \"{workpiece}\";\n"))
            .expect("the parked Construct writes its own approved surface");
    }

    for (index, workpiece) in [FIRST, SECOND].iter().enumerate() {
        let order = constructs.iter().find(|order| order.workpiece == *workpiece).expect("member Construct order");
        harness.release_parked_lanes(from_ref(order));
        harness.pump_until("the captured candidate queues while verification service is held", |harness| {
            harness.commission_store().queued_member_verifications().expect("logical verification requests read").len()
                == index + 1
        });
    }
    harness.hold_member_verification(false);
}

#[test]
fn a_policy_bloom_on_a_landed_head_does_not_reprove_the_base() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then_for(FIRST, StageId::Construct, LaneMode::NeverExits).then_for(
        SECOND,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        .script(&script)
        .start("policy-bloom-on-a-landed-head");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first, second]);
    let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);

    let first_seal = seal_effects(&harness).pop().expect("the first seal journals");
    assert!(dispatched_base_verify(&first_seal), "a head with no receipt still dispatches one: {first_seal:?}");

    complete_joint_constructions(&mut harness);
    harness.pump_until("the contextual bloom resolves its integration root", |harness| {
        replay_snapshot(&mut harness.commission_store())
            .blooms
            .get(&bloom)
            .is_some_and(|record| record.resolved_head.is_some())
    });
    harness.await_landing(bloom, BloomStatus::Landed);

    let new_head = harness.view().mainline;
    let snapshot = replay_snapshot(&mut harness.commission_store());
    let record = snapshot.blooms.get(&bloom).expect("the landed bloom remains projected");
    let resolved_tree = record.resolved_tree.expect("a landed bloom holds its resolved tree");
    let effects = land_effects(&harness, bloom);
    let receipt = effects
        .iter()
        .find_map(|effect| match effect {
            Decision::RecordBaseReceipt { receipt } => Some(receipt),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a contextual landing files its own base receipt: {effects:?}"));
    assert_eq!(receipt.base, new_head, "the receipt is keyed by the head this land produced");
    assert_eq!(receipt.tree, resolved_tree, "the receipt carries the integrated tree the gates proved");
    assert!(receipt.is_green(), "the landed receipt is green: {receipt:?}");

    let next_scope = harness.author_scope_revision(NEXT, &["crates/example-a/**"]);
    approve_scopes(&harness, &[next_scope]);
    let _next = harness.seal_members(&[(NEXT, next_scope)]);
    let second_seal = seal_effects(&harness).pop().expect("the policy seal journals");
    assert!(
        !dispatched_base_verify(&second_seal),
        "a policy bloom on a landed head must not re-prove the base: {second_seal:?}"
    );
    assert!(
        queued_construction(&second_seal),
        "the landed head's receipt admits construct in the same decision set: {second_seal:?}"
    );
}
