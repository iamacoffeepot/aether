//! A member verification request that waited in the queue longer than its own
//! wall clock must still get a lane, and the members queued behind it must
//! still be scheduled after it.
//!
//! Pre-fix the queue row's `deadline_unix_millis` — minted at enqueue as
//! `queued + wall_clock_secs` — was copied verbatim onto the shared run's
//! member rows at dispatch, so a serial run inherited whatever budget the wait
//! had already spent. A request that outwaited its own clock produced a run
//! `next_initial_step` could find no eligible member for: zero steps, zero
//! member outcomes, straight to `Completing`, and a `SharedRunCompleted`
//! carrying empty `outcomes` with the request back in `unfinished`. On the
//! fleet (bloom 9680c483, #6053) that was four runs prepared, started and
//! completed inside a second apiece with no lane, no outcome and no
//! diagnostic, and the same phantom lap replayed at every boot.

#![allow(clippy::unwrap_used)]

use std::thread;
use std::time::Duration;

use aether_bloomery::{
    CoordinationPolicy, Digest, FakeKeyProvider, KeyId, MemberVerifyOutcome, SharedRunDispatch, VerificationMode,
    signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::LaneScript;
use aether_chassis_bloomery::store::{CommissionBackend, SharedRunLifecycle, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

/// Short enough that the hold below outlives it, long enough that a lane
/// dispatched under a freshly minted budget finishes well inside it.
const WALL_CLOCK_SECS: u64 = 6;

const MEMBERS: [(&str, &str); 3] =
    [("wp-a", "crates/example-a/**"), ("wp-b", "crates/example-b/**"), ("wp-c", "crates/example-shared/**")];

fn contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: false,
        max_run_members: 1,
        max_serial_requests: 1,
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
        let approval = signed_approval(KeyId(String::from("queue-wait harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
    }
}

/// Every run that reached a terminal lifecycle with no step of its own and a
/// member still carrying no outcome: a physical run that took a plan, admitted
/// a completion, and answered nothing about the member it was for.
fn phantom_runs(harness: &ScenarioHarness) -> Vec<String> {
    let mut store = harness.commission_store();
    let mut phantoms = Vec::new();
    for run in store.list_shared_runs().expect("shared runs read") {
        if !matches!(run.lifecycle, SharedRunLifecycle::Completing | SharedRunLifecycle::Completed)
            || !store.shared_run_steps(&run.run).expect("shared run steps read").is_empty()
        {
            continue;
        }
        let members = store.shared_run_members(&run.run).expect("shared run members read");
        if members.iter().any(|member| !member.cancelled && member.outcome.is_none()) {
            phantoms.push(format!("nonce={} lifecycle={:?} members={}", run.nonce, run.lifecycle, members.len()));
        }
    }
    phantoms
}

/// The workpieces a shared run has answered for — a member row carrying a
/// decodable outcome, whatever that outcome says about the candidate.
fn answered_members(harness: &ScenarioHarness) -> Vec<String> {
    let mut store = harness.commission_store();
    let mut answered = Vec::new();
    for run in store.list_shared_runs().expect("shared runs read") {
        let Ok(dispatch) = from_bytes::<SharedRunDispatch>(&run.dispatch) else {
            continue;
        };
        for member in store.shared_run_members(&run.run).expect("shared run members read") {
            if member.outcome.as_deref().and_then(|bytes| from_bytes::<MemberVerifyOutcome>(bytes).ok()).is_none() {
                continue;
            }
            if let Some(request) =
                dispatch.plan.requests.iter().find(|request| member.request == request.digest().as_bytes())
            {
                answered.push(request.member.workpiece.0.clone());
            }
        }
    }
    answered.sort_unstable();
    answered.dedup();
    answered
}

/// Every terminal run's step count, so a run that answered for its member
/// without ever running a lane is visible beside one that did.
fn terminal_run_steps(harness: &ScenarioHarness) -> Vec<(String, usize)> {
    let mut store = harness.commission_store();
    let mut rows = Vec::new();
    for run in store.list_shared_runs().expect("shared runs read") {
        if !matches!(run.lifecycle, SharedRunLifecycle::Completing | SharedRunLifecycle::Completed) {
            continue;
        }
        rows.push((run.nonce.clone(), store.shared_run_steps(&run.run).expect("shared run steps read").len()));
    }
    rows
}

#[test]
fn a_run_whose_request_waited_out_the_queue_still_dispatches() {
    let authority = Repo::with_formatted_example_project();
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        // Two provers, stated rather than measured: the base verify keeps one
        // warm for the life of the harness, so the three members share the one
        // remaining slot and the two behind the leader really do wait for it.
        .max_concurrent_provers(2)
        .wall_clock_secs(WALL_CLOCK_SECS)
        .script(&LaneScript::all_passing())
        .start("a-run-whose-request-waited-out-the-queue-still-dispatches");
    // Held from before the seal, so every candidate this bloom produces reaches
    // the durable queue and none of them reaches a plan.
    harness.hold_member_verification(true);
    let scopes: Vec<Digest> =
        MEMBERS.iter().map(|(workpiece, surface)| harness.author_scope_revision(workpiece, &[*surface])).collect();
    approve_scopes(&harness, &scopes);
    let sealed: Vec<(&str, Digest)> =
        MEMBERS.iter().zip(&scopes).map(|((workpiece, _), scope)| (*workpiece, *scope)).collect();
    let _bloom = harness.seal_members(&sealed);

    harness.pump_until("every member's verification request is queued and unscheduled", |harness| {
        let mut store = harness.commission_store();
        assert!(store.list_shared_runs().expect("shared runs read while service is held").is_empty());
        store.queued_member_verifications().expect("logical verification requests read").len() == MEMBERS.len()
    });

    // The wait itself: every queued row outlives the wall clock it was minted
    // with while no run exists to spend that clock.
    thread::sleep(Duration::from_secs(WALL_CLOCK_SECS + 1));
    harness.hold_member_verification(false);

    // Either every request proved, or a run already completed answering nothing
    // about its member — which is the defect, and worth failing on the spot
    // rather than after a budget runs out.
    // Either every request has been answered for, or a run already completed
    // answering nothing about its member — which is the defect, and worth
    // failing on the spot rather than after a budget runs out.
    harness.pump_until("every queued request reached a lane, or a run completed without one", |harness| {
        answered_members(harness).len() >= MEMBERS.len() || !phantom_runs(harness).is_empty()
    });
    let phantoms = phantom_runs(&harness);
    assert!(
        phantoms.is_empty(),
        "a run whose request outwaited its queue budget must dispatch a lane or record why it could not, never \
         complete silently: {phantoms:?}"
    );

    let answered = answered_members(&harness);
    let expected = MEMBERS.iter().map(|(workpiece, _)| (*workpiece).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        answered, expected,
        "the leader runs and every member queued behind it is scheduled and answered for in turn",
    );

    let steps = terminal_run_steps(&harness);
    assert!(
        steps.iter().all(|(_, steps)| *steps > 0),
        "every terminal run spent its physical run on a lane: {steps:?}"
    );
}
