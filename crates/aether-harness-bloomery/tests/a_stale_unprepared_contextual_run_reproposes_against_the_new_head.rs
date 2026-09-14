//! A Contextual proposal that is still preparing when the head advances must be
//! re-proposed against the new head. Without a retry on that stale preparation,
//! the member sits at Verify with a terminal run and a dead queue row.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{
    BloomId, CoordinationPolicy, Digest, Fact, FakeKeyProvider, IntegrationHead, KeyId, MemberPin, ResolvedConfigs,
    SharedRunPhase, SharedRunPlan, Snapshot, StageId, VerificationMode, decode_recorded_decisions,
    decode_recorded_event, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

const FIRST: &str = "wp-a";
const SECOND: &str = "wp-b";
const POLL: Duration = Duration::from_millis(20);
const STEP_BUDGET: Duration = Duration::from_secs(20);

fn eager_contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        verification: VerificationMode::Contextual,
        eager_integration: true,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 2,
        movement_budget: 2,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("stale-prep harness")), &[0x0A; 32], *scope_revision);
        store
            .insert_approval(&approval, &FakeKeyProvider)
            .expect("the original member scope retains its signed approval");
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

fn head(harness: &ScenarioHarness, bloom: BloomId) -> IntegrationHead {
    let snapshot = replay_snapshot(&mut harness.commission_store());
    snapshot
        .blooms
        .get(&bloom)
        .expect("the sealed bloom projects")
        .coordination
        .as_ref()
        .expect("coordination")
        .integration
        .head
        .clone()
}

fn covers(coverage: &[MemberPin], workpiece: &str) -> bool {
    coverage.iter().any(|pin| pin.workpiece.0 == workpiece)
}

fn proposed_plans(harness: &ScenarioHarness) -> Vec<SharedRunPlan> {
    facts(harness, |fact| match fact {
        Fact::ProposeSharedRun { plan, .. } => Some(plan.clone()),
        _ => None,
    })
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

fn dispatch_until(harness: &mut ScenarioHarness, what: &str, pred: impl Fn(&mut ScenarioHarness) -> bool) {
    let deadline = Instant::now() + STEP_BUDGET;
    loop {
        harness.dispatch_tick();
        if pred(harness) {
            return;
        }
        assert!(Instant::now() < deadline, "{what} did not happen inside {STEP_BUDGET:?}");
        thread::sleep(POLL);
    }
}

#[test]
fn a_stale_unprepared_contextual_run_reproposes_against_the_new_head() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then_for(FIRST, StageId::Construct, LaneMode::NeverExits).then_for(
        SECOND,
        StageId::Construct,
        LaneMode::NeverExits,
    );
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(eager_contextual_policy())
        .script(&script)
        .start("stale-unprepared-contextual-run");
    let first = harness.author_scope_revision(FIRST, &["crates/example-a/**"]);
    let second = harness.author_scope_revision(SECOND, &["crates/example-b/**"]);
    approve_scopes(&harness, &[first, second]);
    let bloom = harness.seal_members(&[(FIRST, first), (SECOND, second)]);
    complete_joint_constructions(&mut harness);

    dispatch_until(&mut harness, "both members are proposed before either prepares", |harness| {
        proposed_plans(harness).len() == 2
    });
    let initial = proposed_plans(&harness);
    assert!(
        initial
            .iter()
            .all(|plan| { plan.composition.as_ref().is_some_and(|composition| composition.base.coverage.is_empty()) }),
        "both proposals stand on the sealed base: {initial:?}"
    );

    harness.integrate_tick();
    dispatch_until(&mut harness, "the first proposed run completes before the sibling prepares", |harness| {
        facts(harness, |fact| match fact {
            Fact::SharedRunCompleted { completion, .. } => Some(completion.plan),
            _ => None,
        })
        .len()
            == 1
    });

    let lagging = {
        let completed = facts(&harness, |fact| match fact {
            Fact::SharedRunCompleted { completion, .. } => Some(completion.plan),
            _ => None,
        });
        let leading_plan =
            initial.iter().find(|plan| completed.contains(&plan.digest())).expect("the completed run was proposed");
        let leading = leading_plan.requests[0].member.workpiece.0.as_str();
        if leading == FIRST {
            SECOND
        } else {
            FIRST
        }
    };

    let snapshot = replay_snapshot(&mut harness.commission_store());
    let state = snapshot.blooms.get(&bloom).expect("bloom").coordination.as_ref().expect("coordination");
    assert!(
        state.runs.iter().any(|run| {
            run.phase == SharedRunPhase::Preparing
                && run.plan.requests.iter().any(|request| request.member.workpiece.0 == lagging)
        }),
        "the sibling is still preparing when the first run settles: {:?}",
        state
            .runs
            .iter()
            .map(|run| (
                &run.phase,
                run.plan.requests.iter().map(|request| request.member.workpiece.0.as_str()).collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>()
    );

    harness.integrate_tick();
    harness.pump_until("the displaced member is proposed against the head that moved under it", |harness| {
        let head = head(harness, bloom);
        covers(
            &head.coverage,
            if lagging == FIRST {
                SECOND
            } else {
                FIRST
            },
        ) && proposed_plans(harness).iter().any(|plan| {
            plan.requests.iter().any(|request| request.member.workpiece.0 == lagging)
                && plan.composition.as_ref().is_some_and(|composition| composition.base == head)
        })
    });

    let head = head(&harness, bloom);
    assert!(covers(
        &head.coverage,
        if lagging == FIRST {
            SECOND
        } else {
            FIRST
        }
    ));
    assert!(
        proposed_plans(&harness).iter().any(|plan| {
            plan.requests.iter().any(|request| request.member.workpiece.0 == lagging)
                && plan.composition.as_ref().is_some_and(|composition| composition.base.node == head.node)
        }),
        "the lagging member's replacement proposal must name the head that displaced it"
    );
}
