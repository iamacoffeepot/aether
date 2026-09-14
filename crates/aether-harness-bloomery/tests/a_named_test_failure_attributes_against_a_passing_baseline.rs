//! One member, one failing test, a passing baseline: attribution must settle
//! instead of re-probing the same check under nextest's progress counter.
//!
//! Pre-fix: the observation key embedded `(n/m)`, a green baseline recorded no
//! per-test outcomes, and `observed_probe_verdict` could not answer `Passed`
//! for the named test. The walk then spent the probe budget on full-suite
//! reruns rather than attributing the singleton.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use aether_bloomery::{
    CoordinationPolicy, Digest, FailureScope, FakeKeyProvider, KeyId, MemberVerifyOutcome, SharedRunDispatch,
    SharedRunMode, VERIFY_CHECK_COMMAND, VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::bloomery::{BatchCheck, BatchProbeRequest};
use aether_chassis_bloomery::store::{CommissionBackend, SharedRunLifecycle, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};
use serde::Deserialize;

const MEMBER: &str = "wp-a";

fn contextual_policy() -> CoordinationPolicy {
    CoordinationPolicy {
        verification: VerificationMode::Contextual,
        eager_integration: false,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 8,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
    }
}

fn approve_scope(harness: &ScenarioHarness, scope_revision: Digest) {
    let mut store = harness.commission_store();
    let approval = signed_approval(KeyId(String::from("named-test harness")), &[0x0A; 32], scope_revision);
    store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
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
    // Occurrence 0 is the contextual candidate, 1–2 are the green baseline
    // pair, 3 is the second independent full-set receipt. Baselines now share
    // `verify.check` with the candidate (#5944), so they consume occurrences.
    let script = LaneScript::all_passing()
        .then(VERIFY_CHECK_COMMAND, LaneMode::Fail)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Pass)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Pass)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Fail);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        .script(&script)
        .start("named-test-attribution");
    let scope = harness.author_scope_revision(MEMBER, &["mock-lane-candidate.txt"]);
    approve_scope(&harness, scope);
    let bloom = harness.seal_member(MEMBER, scope);

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
        2,
        "the one experiment still takes the two independent receipts discrimination demands: {baseline_probes:?}"
    );
    assert!(
        matches!(baseline.iter().next(), Some(BatchCheck::Test { id, .. }) if id.contains("named_failure")),
        "attribution is of the named test, not the gate: {baseline:?}"
    );
    assert_eq!(harness.bloom(bloom).members.len(), 1);
}
