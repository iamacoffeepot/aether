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
        red_verify: aether_bloomery::RedVerify::Refine,
        verification: VerificationMode::Contextual,
        eager_integration: false,
        max_run_members: 1,
        max_serial_requests: 1,
        max_attribution_probes: 8,
        movement_budget: 1,
        reservation_millis: 1_000,
        host_class: "harness".to_owned(),
        coalesce_millis: None,
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
    // Occurrence 0 is the contextual candidate and 1 is the green baseline.
    // Baselines share `verify.check` with the candidate (#5944), so they
    // consume occurrences; one apiece since an experiment stopped taking two
    // invocations (ADR-0218, amended 2026-09-15).
    let script =
        LaneScript::all_passing().then(VERIFY_CHECK_COMMAND, LaneMode::Fail).then(VERIFY_CHECK_COMMAND, LaneMode::Pass);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        // Two prover slots, stated rather than measured: the base verify keeps
        // one warm, so a host whose measured ceiling is one (cores / 8) never
        // frees a slot for the contextual run this scenario is about.
        .max_concurrent_provers(2)
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
        1,
        "one experiment is one invocation since ADR-0218's 2026-09-15 amendment: {baseline_probes:?}"
    );
    assert!(
        matches!(baseline.iter().next(), Some(BatchCheck::Test { id, .. }) if id.contains("named_failure")),
        "attribution is of the named test, not the gate: {baseline:?}"
    );
    assert_eq!(harness.bloom(bloom).members.len(), 1);
}
