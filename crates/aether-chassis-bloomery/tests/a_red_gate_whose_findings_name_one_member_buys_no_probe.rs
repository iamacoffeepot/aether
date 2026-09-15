//! Two members in one contextual node, one red gate, and a diagnostic that
//! names a file only one of them changed: the run must eject that member on the
//! step-0 evidence alone and buy no probe at all.
//!
//! This is ADR-0218's amendment of 2026-09-15 end to end. Before it, the
//! planner treated the gate as a black box that says only red or green for a
//! set, so it bought the empty baseline, the whole set, each half, each
//! singleton, and each inherited head — twice over — at four to six minutes a
//! step. Bloom 0f16e207's run 84DB66FF spent 37.6 minutes of that on a
//! `verify.suppress` red whose two findings lines already named the two files
//! that caused it, while the innocent sibling's own gates had been green for
//! fifty minutes.
//!
//! The assertion that matters is the negative one: **zero probe steps**. A
//! version that attributes correctly and still buys a confirming bisection has
//! fixed nothing, and every other assertion here would pass under it.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    CONSTRUCT_IMPLEMENT_COMMAND, CoordinationPolicy, Digest, FailureScope, FakeKeyProvider, KeyId, MemberVerifyOutcome,
    SharedRunDispatch, SharedRunMode, VERIFY_CHECK_COMMAND, VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, SharedRunLifecycle, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};

/// The member whose file the failing gate's findings name.
const CULPRIT: &str = "wp-a";
/// The member that changed nothing the findings mention.
const INNOCENT: &str = "wp-b";

const MEMBERS: [(&str, &str, &str); 2] = [
    (CULPRIT, "crates/example-a/**", "crates/example-a/src/contextual.rs"),
    (INNOCENT, "crates/example-b/**", "crates/example-b/src/contextual.rs"),
];

/// The step-0 findings, in the shape run 84DB66FF's suppression scanner
/// reported: one section headed by the gate, one unindented line per location.
const FINDINGS: &str = "\
The previous candidate failed verification. Fix these.

### verify.suppress

crates/example-a/src/contextual.rs:1 — ignore — #[ignore = \"scripted\"]
";

fn coalescing_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        verification: VerificationMode::Contextual,
        eager_integration: true,
        max_run_members: 2,
        max_serial_requests: 2,
        // Deliberately generous. A budget of zero would make "no probe ran" a
        // statement about the budget rather than about the attribution.
        max_attribution_probes: 8,
        movement_budget: 2,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        // The coalescing wall-clock hold is not what this scenario is about,
        // and a scenario that waited it out would be measuring the clock. The
        // two requests are made simultaneous by holding the verification gate
        // until both constructs have captured, so the scheduler sees them
        // together with no hold to expire.
        coalesce_millis: Some(0),
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval =
            signed_approval(KeyId(String::from("findings-attribution harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
    }
}

/// Park both members in Construct, then give each one the file its own surface
/// covers, so the composition's two candidates have disjoint deltas and a
/// diagnostic naming one of those files has exactly one owner.
fn park_constructs(harness: &mut ScenarioHarness) -> Vec<OutstandingOrder> {
    harness.hold_member_verification(true);
    harness.pump_until("both members receive their parked Construct orders", |harness| {
        let orders = harness.orders();
        assert!(orders.len() <= 2, "only the two sealed members can be dispatched: {orders:?}");
        orders.len() == 2
    });
    let constructs = harness.orders();
    harness.pump_until("both parked Construct children reached their real worktrees", |harness| {
        let runs = harness.ledger();
        constructs.iter().all(|order| runs.iter().any(|run| run.nonce == order.nonce))
    });

    let runs = harness.ledger();
    for (workpiece, _, path) in MEMBERS {
        let order = constructs.iter().find(|order| order.workpiece == workpiece).expect("sealed member Construct");
        let worktree = runs
            .iter()
            .find(|run| run.nonce == order.nonce)
            .and_then(|run| run.worktree.as_deref())
            .map(Path::new)
            .expect("the parked local Construct recorded its real worktree");
        fs::remove_file(worktree.join(CANDIDATE_FILE)).expect("the mock's generic candidate is removed");
        fs::write(worktree.join(path), format!("pub const MEMBER: &str = \"{workpiece}\";\n"))
            .expect("the parked Construct writes inside its own approved surface");
    }
    constructs
}

fn queued_verifies(harness: &ScenarioHarness) -> usize {
    harness.commission_store().queued_member_verifications().expect("logical verification requests read").len()
}

fn shared_run_members(harness: &ScenarioHarness) -> Vec<Vec<String>> {
    harness
        .commission_store()
        .list_shared_runs()
        .expect("shared runs read")
        .iter()
        .filter_map(|run| from_bytes::<SharedRunDispatch>(&run.dispatch).ok())
        .map(|dispatch| dispatch.plan.requests.iter().map(|request| request.member.workpiece.0.clone()).collect())
        .collect()
}

fn contextual_run(harness: &ScenarioHarness) -> Option<(Vec<u8>, Vec<MemberVerifyOutcome>)> {
    let mut store = harness.commission_store();
    store.list_shared_runs().ok()?.into_iter().find_map(|run| {
        let dispatch = from_bytes::<SharedRunDispatch>(&run.dispatch).ok()?;
        if dispatch.plan.mode != SharedRunMode::Contextual || run.lifecycle != SharedRunLifecycle::Completed {
            return None;
        }
        let outcomes = store
            .shared_run_members(&run.run)
            .ok()?
            .iter()
            .filter_map(|member| member.outcome.as_deref())
            .filter_map(|bytes| from_bytes::<MemberVerifyOutcome>(bytes).ok())
            .collect::<Vec<_>>();
        (outcomes.len() == 2).then_some((run.run, outcomes))
    })
}

fn probe_steps(harness: &ScenarioHarness, run: &[u8]) -> usize {
    harness
        .commission_store()
        .shared_run_steps(run)
        .expect("the completed run's steps read")
        .iter()
        .filter_map(|step| serde_json::from_slice::<serde_json::Value>(&step.descriptor).ok())
        .filter(|descriptor| descriptor.get("Probe").is_some())
        .count()
}

fn outcome_for<'a>(
    harness: &ScenarioHarness,
    outcomes: &'a [MemberVerifyOutcome],
    workpiece: &str,
) -> &'a MemberVerifyOutcome {
    let mut store = harness.commission_store();
    let run = store
        .list_shared_runs()
        .expect("shared runs read")
        .into_iter()
        .find(|run| run.lifecycle == SharedRunLifecycle::Completed)
        .expect("the contextual run completed");
    let dispatch = from_bytes::<SharedRunDispatch>(&run.dispatch).expect("the completed dispatch decodes");
    let request = dispatch
        .plan
        .requests
        .iter()
        .find(|request| request.member.workpiece.0 == workpiece)
        .expect("the member is in the composition")
        .digest();
    outcomes.iter().find(|outcome| outcome.request() == request).expect("the member settled")
}

#[test]
fn a_red_gate_whose_findings_name_one_member_buys_no_probe() {
    let authority = Repo::with_formatted_example_project();
    // Unkeyed steps throughout: a script carrying any workpiece-keyed step
    // selects by axis alone and would never reach the `verify.check` step this
    // scenario turns on. Both constructs park so each member's own file can be
    // written into its worktree; the first contextual verify then fails.
    let script = LaneScript::all_passing()
        .then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits)
        .then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
        .reporting("verify.suppress", FINDINGS);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(coalescing_policy())
        // Two prover slots, stated rather than measured: the base verify keeps
        // one warm, so a host whose measured ceiling is one would never free a
        // slot for the contextual run this scenario is about.
        .max_concurrent_provers(2)
        .script(&script)
        .start("findings-attribution-spends-no-probe");

    let scopes = MEMBERS
        .iter()
        .map(|(workpiece, surface, _)| (*workpiece, harness.author_scope_revision(workpiece, from_ref(surface))))
        .collect::<Vec<_>>();
    approve_scopes(&harness, &scopes.iter().map(|(_, revision)| *revision).collect::<Vec<_>>());
    let bloom = harness.seal_members(&scopes);

    let constructs = park_constructs(&mut harness);
    harness.release_parked_lanes(&constructs);
    harness.pump_until("both captured candidates queue a member verification", |harness| queued_verifies(harness) == 2);
    harness.hold_member_verification(false);
    harness.pump_until("one contextual plan carries both members", |harness| {
        shared_run_members(harness).iter().any(|members| members.len() == 2)
    });
    harness
        .pump_until("the coalesced contextual run settles both members", |harness| contextual_run(harness).is_some());

    let (run, outcomes) = contextual_run(&harness).expect("the contextual run settled");
    assert_eq!(
        probe_steps(&harness, &run),
        0,
        "the findings named a file one member changed, so the run owes no bisection",
    );

    assert!(
        matches!(
            outcome_for(&harness, &outcomes, CULPRIT),
            MemberVerifyOutcome::Failed { scope: FailureScope::Attributed { members, .. }, .. }
                if members.len() == 1 && members[0].workpiece.0 == CULPRIT
        ),
        "the member whose file the diagnostic named is charged with the gate: {outcomes:?}",
    );
    assert!(
        matches!(outcome_for(&harness, &outcomes, INNOCENT), MemberVerifyOutcome::Survived { .. }),
        "the sibling the findings never mention survives the node and carries into the fresh one: {outcomes:?}",
    );

    let projected = harness
        .commission_store()
        .lookup_review_findings(bloom.0.as_bytes(), CULPRIT)
        .expect("the member findings row reads")
        .expect("an attributed member carries the run's findings");
    assert!(
        projected.starts_with("Attributed from the step-0 findings by changed path:"),
        "the evidence says how the member was named, so a repair lap is not left guessing: {projected}",
    );
    assert!(projected.contains("crates/example-a/src/contextual.rs"), "and which path named it: {projected}");
}
