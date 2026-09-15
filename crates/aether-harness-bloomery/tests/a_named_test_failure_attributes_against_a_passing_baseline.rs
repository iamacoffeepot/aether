//! One failing test, a passing baseline: attribution must settle instead of
//! re-probing the same check under nextest's progress counter.
//!
//! Pre-fix: the observation key embedded `(n/m)`, a green baseline recorded no
//! per-test outcomes, and `observed_probe_verdict` could not answer `Passed`
//! for the named test. The walk then spent the probe budget on full-suite
//! reruns rather than attributing the singleton.
//!
//! The composition is two members because a bisection is only ever bought for
//! one (#6054): a run with a single member is attributed from its findings and
//! issues no probe, so a single-member fixture would prove nothing about the
//! baseline's identity.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    CONSTRUCT_IMPLEMENT_COMMAND, CoordinationPolicy, Digest, FailureScope, FakeKeyProvider, KeyId, MemberVerifyOutcome,
    SharedRunDispatch, SharedRunMode, VERIFY_CHECK_COMMAND, VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::bloomery::{BatchCheck, BatchProbeRequest};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, SharedRunLifecycle, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};
use serde::Deserialize;

const MEMBERS: [(&str, &str, &str); 2] = [
    ("wp-a", "crates/example-a/**", "crates/example-a/src/contextual.rs"),
    ("wp-b", "crates/example-b/**", "crates/example-b/src/contextual.rs"),
];

fn contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: false,
        max_run_members: 2,
        max_serial_requests: 2,
        max_attribution_probes: 8,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: Some(0),
    }
}

fn approve_scopes(harness: &ScenarioHarness, scope_revisions: &[Digest]) {
    let mut store = harness.commission_store();
    for scope_revision in scope_revisions {
        let approval = signed_approval(KeyId(String::from("named-test harness")), &[0x0A; 32], *scope_revision);
        store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
    }
}

/// Park both members in Construct and give each the file its own surface
/// covers, so the two candidates fold into one node without colliding.
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

#[derive(Deserialize)]
enum Descriptor {
    Probe(ProbePreparation),
}

#[derive(Deserialize)]
struct ProbePreparation {
    probe: BatchProbeRequest,
}

#[test]
fn a_named_test_failure_attributes_against_a_passing_baseline() {
    let authority = Repo::with_formatted_example_project();
    // Baselines share `verify.check` with the candidate (#5944), so they
    // consume occurrences; one apiece since an experiment stopped taking two
    // invocations (ADR-0218, amended 2026-09-15). In order: the contextual
    // candidate, the green baseline, the pair, and the first half.
    let script = LaneScript::all_passing()
        .then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits)
        .then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Pass)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Fail);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        // Two prover slots, stated rather than measured: the base verify keeps
        // one warm, so a host whose measured ceiling is one (cores / 8) never
        // frees a slot for the contextual run this scenario is about.
        .max_concurrent_provers(2)
        .script(&script)
        .start("named-test-attribution");
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

    harness.pump_until("the singleton failing test is attributed", |harness| {
        let mut store = harness.commission_store();
        store.list_shared_runs().ok().into_iter().flatten().any(|run| {
            run.lifecycle == SharedRunLifecycle::Completed
                && from_bytes::<SharedRunDispatch>(&run.dispatch)
                    .is_ok_and(|dispatch| dispatch.plan.mode == SharedRunMode::Contextual)
                && store.shared_run_members(&run.run).ok().into_iter().flatten().any(|member| {
                    member.outcome.as_deref().is_some_and(|bytes| {
                        matches!(
                            from_bytes::<MemberVerifyOutcome>(bytes),
                            Ok(MemberVerifyOutcome::Failed { scope: FailureScope::Attributed { .. }, .. })
                        )
                    })
                })
        })
    });

    let mut store = harness.commission_store();
    let run = store
        .list_shared_runs()
        .expect("shared runs read after attribution")
        .into_iter()
        .find(|run| run.lifecycle == SharedRunLifecycle::Completed)
        .expect("the contextual run completed");
    let mut baseline = BTreeSet::new();
    let mut baseline_probes = Vec::new();
    for step in store.shared_run_steps(&run.run).expect("shared steps read") {
        let Ok(Descriptor::Probe(preparation)) = serde_json::from_slice(&step.descriptor) else {
            continue;
        };
        if !preparation.probe.members.is_empty() || preparation.probe.baseline.is_some() {
            continue;
        }
        baseline.insert(preparation.probe.check.clone());
        baseline_probes.push(preparation.probe);
    }
    assert_eq!(
        baseline.len(),
        1,
        "one named test is one baseline experiment, not a new check per suite size: {baseline_probes:?}"
    );
    assert_eq!(
        baseline_probes.len(),
        1,
        "one experiment is one invocation since ADR-0218's 2026-09-15 amendment: {baseline_probes:?}"
    );
    assert!(
        matches!(baseline.iter().next(), Some(BatchCheck::Test { id, .. }) if id.contains("named_failure")),
        "attribution is of the named test, not the gate: {baseline:?}"
    );
    assert_eq!(harness.bloom(bloom).members.len(), 2);
}
