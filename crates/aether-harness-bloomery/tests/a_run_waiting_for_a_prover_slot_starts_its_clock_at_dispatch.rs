//! A shared run's sealed wall clock is a budget for running, and it starts at
//! the dispatch that spends it.
//!
//! Pre-fix `drain_shared_dispatches` minted `deadline_unix_millis` when the run
//! was *prepared*. A run prepared while the prover ceiling is full then sat
//! `Ready` on someone else's lane with its own clock running, and on bloom
//! 0c5a157e that produced both halves of the same defect: `dispatch-8161`'s two
//! requests, queued 21:19:38Z with a 21:36:43Z deadline, were written off at
//! that deadline while still undispatched and the run was dispatched anyway at
//! 22:01Z, spending a prover slot and a whole-workspace doc-plus-clippy pass on
//! a verdict nothing would consume; `dispatch-8333-step-1` was dispatched at
//! 22:58Z and cancelled at 23:02:18Z — four minutes into a fifteen-minute
//! budget — and its member was charged a host fault for it (#6073).
//!
//! Both scenarios share one fixture: the leading member's verify lane parks on
//! the only prover slot the host has, so the trailing member's run is prepared
//! and then waits — the exact window the fleet spent forty minutes in.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::slice::from_ref;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, thread};

use aether_bloomery::{
    CoordinationPolicy, Digest, FakeKeyProvider, KeyId, MemberVerifyOutcome, SharedRunDispatch, StageId,
    VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

/// Short enough that the wait in front of the trailing member outlives it, long
/// enough that a lane dispatched under a freshly started clock finishes well
/// inside it.
const WALL_CLOCK_SECS: u64 = 6;

const LEADER: &str = "wp-a";
const TRAILER: &str = "wp-b";

const MEMBERS: [(&str, &str); 2] = [(LEADER, "crates/example-a/**"), (TRAILER, "crates/example-b/**")];

fn standalone_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Standalone,
        eager_integration: false,
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
        let approval = signed_approval(KeyId(String::from("dispatch-clock harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
    }
}

fn now_unix_millis() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).expect("the host clock is past the epoch").as_millis())
        .expect("a Unix millisecond fits a u64")
}

/// Let both members' parked Construct lanes write their own approved surface
/// and release them, so each reaches verification as its own request.
fn complete_constructions(harness: &mut ScenarioHarness) {
    harness.pump_until("both members received a parked Construct lane", |harness| {
        harness.ledger().iter().filter(|run| run.stage == Some(StageId::Construct) && run.worktree.is_some()).count()
            == MEMBERS.len()
    });
    let constructs = harness
        .orders()
        .into_iter()
        .filter(|order| from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::Construct))
        .collect::<Vec<_>>();
    for order in &constructs {
        let worktree = harness
            .ledger()
            .iter()
            .find(|run| run.nonce == order.nonce)
            .and_then(|run| run.worktree.clone())
            .expect("the parked Construct recorded its real worktree");
        let worktree = Path::new(&worktree);
        let path = format!(
            "crates/example-{}/src/contextual.rs",
            if order.workpiece == LEADER {
                "a"
            } else {
                "b"
            }
        );
        fs::remove_file(worktree.join(CANDIDATE_FILE)).expect("the mock's generic candidate is removed");
        fs::write(worktree.join(&path), format!("pub const MEMBER: &str = \"{}\";\n", order.workpiece))
            .expect("the parked Construct writes its own approved surface");
    }
    harness.release_parked_lanes(&constructs);
}

/// Every shared-run member row, named by its workpiece, with the deadline its
/// run carries for it and what its outcome says.
fn member_rows(harness: &ScenarioHarness) -> Vec<(String, u64, Option<String>)> {
    let mut store = harness.commission_store();
    let mut rows = Vec::new();
    for run in store.list_shared_runs().expect("shared runs read") {
        let Ok(dispatch) = from_bytes::<SharedRunDispatch>(&run.dispatch) else {
            continue;
        };
        for member in store.shared_run_members(&run.run).expect("shared run members read") {
            let Some(request) =
                dispatch.plan.requests.iter().find(|request| member.request == request.digest().as_bytes())
            else {
                continue;
            };
            let answer = member.outcome.as_deref().and_then(|bytes| from_bytes::<MemberVerifyOutcome>(bytes).ok()).map(
                |outcome| {
                    match outcome {
                        MemberVerifyOutcome::PassedStandalone { .. } => "passed-standalone",
                        MemberVerifyOutcome::PassedIn { .. } => "passed-in",
                        MemberVerifyOutcome::Failed { .. } => "failed",
                        MemberVerifyOutcome::HostFault { .. } => "host-fault",
                        MemberVerifyOutcome::Survived { .. } => "survived",
                        MemberVerifyOutcome::Pending { .. } => "pending",
                    }
                    .to_owned()
                },
            );
            rows.push((request.member.workpiece.0.clone(), member.deadline_unix_millis, answer));
        }
    }
    rows
}

fn verify_lanes(harness: &ScenarioHarness, workpiece: &str) -> usize {
    harness
        .ledger()
        .iter()
        .filter(|run| run.stage == Some(StageId::Verify) && run.workpiece.as_deref() == Some(workpiece))
        .count()
}

/// Seal two members, let both constructs land their own surface, and park the
/// leader's verify on the only prover slot — leaving the trailer's run prepared
/// and waiting with no lane of its own.
fn a_leader_holding_the_only_prover_slot(client_name: &str, authority: &Repo) -> ScenarioHarness {
    let script = LaneScript::all_passing()
        .then_for(LEADER, StageId::Construct, LaneMode::NeverExits)
        .then_for(TRAILER, StageId::Construct, LaneMode::NeverExits)
        .then_for(LEADER, StageId::Verify, LaneMode::NeverExits);
    let mut harness = HarnessBuilder::local_authority(authority)
        .coordination(standalone_policy())
        // Two provers, stated rather than measured: the base verify keeps one
        // warm for the life of the harness, so the leader's parked verify holds
        // the one remaining slot and the trailer really does wait for it.
        .max_concurrent_provers(2)
        .wall_clock_secs(WALL_CLOCK_SECS)
        .script(&script)
        .start(client_name);
    let scopes: Vec<Digest> =
        MEMBERS.iter().map(|(workpiece, surface)| harness.author_scope_revision(workpiece, &[*surface])).collect();
    approve_scopes(&harness, &scopes);
    let sealed: Vec<(&str, Digest)> =
        MEMBERS.iter().zip(&scopes).map(|((workpiece, _), scope)| (*workpiece, *scope)).collect();
    let _bloom = harness.seal_members(&sealed);
    complete_constructions(&mut harness);

    harness.pump_until("the leader holds the only prover slot and the trailer has a run waiting for it", |harness| {
        verify_lanes(harness, LEADER) == 1
            && harness.commission_store().list_shared_runs().expect("shared runs read").len() == MEMBERS.len()
    });
    assert_eq!(verify_lanes(&harness, TRAILER), 0, "the trailer cannot have a lane while the leader holds the slot");
    harness
}

/// The leader's parked verify, so a scenario can hand the slot on.
fn leader_verify_order(harness: &ScenarioHarness) -> OutstandingOrder {
    harness
        .orders()
        .into_iter()
        .find(|order| {
            from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::Verify) && order.workpiece == LEADER
        })
        .expect("the leader's parked verify is an outstanding order")
}

/// The trailer's run, and the plan digest a cancellation is retained against.
fn trailer_plan(harness: &ScenarioHarness) -> Digest {
    let mut store = harness.commission_store();
    store
        .list_shared_runs()
        .expect("shared runs read")
        .iter()
        .filter_map(|run| from_bytes::<SharedRunDispatch>(&run.dispatch).ok())
        .find(|dispatch| dispatch.plan.requests.iter().any(|request| request.member.workpiece.0 == TRAILER))
        .map(|dispatch| dispatch.plan.digest())
        .expect("the trailer has a shared run of its own")
}

#[test]
fn a_run_that_waited_for_a_prover_slot_runs_under_a_clock_that_starts_at_dispatch() {
    let authority = Repo::with_formatted_example_project();
    let mut harness = a_leader_holding_the_only_prover_slot(
        "a-run-that-waited-for-a-prover-slot-runs-under-a-clock-that-starts-at-dispatch",
        &authority,
    );

    // The wait itself: the trailer's run outlives the whole budget its sealed
    // limits buy while it has not been dispatched and no lane of its own
    // exists to spend it.
    thread::sleep(Duration::from_secs(WALL_CLOCK_SECS + 1));
    let released_at = now_unix_millis();
    harness.release_parked_lanes(from_ref(&leader_verify_order(&harness)));

    harness.pump_until("the trailer's run is answered for", |harness| {
        member_rows(harness).iter().any(|(workpiece, _, answer)| workpiece == TRAILER && answer.is_some())
    });

    let rows = member_rows(&harness);
    let (_, deadline, answer) =
        rows.iter().find(|(workpiece, _, _)| workpiece == TRAILER).expect("the trailer has a member row").clone();

    assert_eq!(
        verify_lanes(&harness, TRAILER),
        1,
        "the trailer's verdict came from a lane it actually ran, not from an expiry receipt: {rows:?}",
    );
    assert!(
        deadline >= released_at,
        "the trailer's wall clock started at the dispatch that spends it, not at the preparation it waited through: \
         deadline {deadline} is before the slot freed at {released_at}",
    );
    assert_eq!(
        answer.as_deref(),
        Some("passed-standalone"),
        "a lane that ran inside its own budget answers about the candidate, never a member host fault: {rows:?}",
    );
}

#[test]
fn a_run_recorded_terminal_before_dispatch_never_reaches_a_lane() {
    // The other half of bloom 0c5a157e: `dispatch-8161` was already terminal
    // when its lane started, and ran thirty-three minutes of docs and clippy
    // over every workspace crate for a verdict the coordinator had already
    // re-queued elsewhere. The cancellation written here is the same durable
    // row the `CancelSharedRun` drain retains, and it is written while the run
    // is still waiting for its slot — so what the dispatch must consult is the
    // journal, not whatever the lifecycle happened to say when the step was
    // planned.
    let authority = Repo::with_formatted_example_project();
    let mut harness = a_leader_holding_the_only_prover_slot(
        "a-run-recorded-terminal-before-dispatch-never-reaches-a-lane",
        &authority,
    );

    let plan = trailer_plan(&harness);
    harness
        .commission_store()
        .record_shared_run_cancellation(plan.as_bytes())
        .expect("the trailer's run is recorded cancelled while it waits for a slot");
    harness.release_parked_lanes(from_ref(&leader_verify_order(&harness)));

    harness.pump_until("the trailer's terminal run is settled", |harness| {
        member_rows(harness).iter().any(|(workpiece, _, answer)| workpiece == TRAILER && answer.is_some())
    });

    let rows = member_rows(&harness);
    assert_eq!(
        verify_lanes(&harness, TRAILER),
        0,
        "a run the journal already recorded terminal must never spend a prover slot on a verdict nobody reads: \
         {rows:?}",
    );
    assert_eq!(verify_lanes(&harness, LEADER), 1, "the leader's own lane is untouched by its sibling's cancellation");
}
