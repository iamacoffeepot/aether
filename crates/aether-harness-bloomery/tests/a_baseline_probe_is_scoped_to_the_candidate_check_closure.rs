//! A named test failure's baseline probe must rerun the candidate check's
//! closure, not the whole workspace.
//!
//! Pre-fix: `probe_transformation` rewrote the empty-subset probe to
//! `verify.base` and cleared `--diff-base`, so attribution re-proved every
//! crate to answer one named test. The inverted range (candidate checkout as
//! `--diff-base`, composition base as HEAD) is the same reverse-dependency
//! closure the candidate check already ran.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    CandidateRef, CoordinationPolicy, Digest, FakeKeyProvider, KeyId, SharedRunDispatch, SharedRunExecution,
    SharedRunMode, Transformation, VERIFY_CHECK_COMMAND, VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::BatchProbeRequest;
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, StoreBackend};
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
    let approval = signed_approval(KeyId(String::from("baseline-closure harness")), &[0x0A; 32], scope_revision);
    store.insert_approval(&approval, &FakeKeyProvider).expect("the member scope retains its signed approval");
}

#[derive(Deserialize)]
enum Descriptor {
    Probe(ProbeRequest),
}

#[derive(Deserialize)]
struct ProbeRequest {
    probe: BatchProbeRequest,
    candidate_checkout: Option<Digest>,
}

#[derive(Deserialize)]
enum Prepared {
    Prepared(PreparedBody),
}

#[derive(Deserialize)]
struct PreparedBody {
    transformation: Transformation,
    candidate: CandidateRef,
}

#[test]
fn a_baseline_probe_is_scoped_to_the_candidate_check_closure() {
    let authority = Repo::with_formatted_example_project();
    let script = LaneScript::all_passing().then(VERIFY_CHECK_COMMAND, LaneMode::Fail);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        .script(&script)
        .start("baseline-probe-closure");
    let scope = harness
        .author_scope_revision(MEMBER, &["mock-lane-candidate.txt", "crates/example-a/**", "crates/example-shared/**"]);
    approve_scope(&harness, scope);
    let bloom = harness.seal_member(MEMBER, scope);

    harness.pump_until("the baseline probe is prepared over the candidate check's closure", |harness| {
        let mut store = harness.commission_store();
        store.list_shared_runs().ok().into_iter().flatten().any(|run| {
            from_bytes::<SharedRunDispatch>(&run.dispatch)
                .is_ok_and(|dispatch| dispatch.plan.mode == SharedRunMode::Contextual)
                && store.shared_run_steps(&run.run).ok().into_iter().flatten().any(|step| {
                    let Ok(Descriptor::Probe(request)) = serde_json::from_slice(&step.descriptor) else {
                        return false;
                    };
                    request.probe.members.is_empty()
                        && request.probe.baseline.is_none()
                        && step.prepared.as_deref().is_some_and(|bytes| {
                            matches!(
                                serde_json::from_slice::<Prepared>(bytes),
                                Ok(Prepared::Prepared(prepared))
                                    if prepared.transformation.command == VERIFY_CHECK_COMMAND
                                        && prepared.transformation.diff_base.is_some()
                            )
                        })
                })
        })
    });

    let mut store = harness.commission_store();
    let run = store
        .list_shared_runs()
        .expect("shared runs read after the baseline is prepared")
        .into_iter()
        .find(|run| {
            from_bytes::<SharedRunDispatch>(&run.dispatch)
                .is_ok_and(|dispatch| dispatch.plan.mode == SharedRunMode::Contextual)
        })
        .expect("the contextual run exists");
    let dispatch = from_bytes::<SharedRunDispatch>(&run.dispatch).expect("the contextual dispatch decodes");
    let SharedRunExecution::Contextual { node, transformation, .. } = dispatch.execution else {
        panic!("the contextual run retains its candidate-check transformation");
    };
    let base = dispatch.plan.composition.as_ref().expect("a contextual plan has a composition").base.candidate;

    let mut scoped = 0;
    for step in store.shared_run_steps(&run.run).expect("shared steps read") {
        let Ok(Descriptor::Probe(request)) = serde_json::from_slice(&step.descriptor) else {
            continue;
        };
        if !request.probe.members.is_empty() || request.probe.baseline.is_some() {
            continue;
        }
        assert_eq!(
            request.candidate_checkout,
            Some(transformation.checkout),
            "the request carries the candidate check's checkout as the closure tip"
        );
        assert_eq!(request.candidate_checkout, Some(node.candidate.checkout));

        let prepared = match serde_json::from_slice(step.prepared.as_deref().expect("the baseline is prepared")) {
            Ok(Prepared::Prepared(prepared)) => prepared,
            Err(error) => panic!("a baseline probe prepares: {error}"),
        };
        assert_eq!(
            prepared.transformation.command, VERIFY_CHECK_COMMAND,
            "attribution keeps the candidate check, not the whole-workspace base command"
        );
        assert_eq!(prepared.candidate, base, "the probe runs at the composition base");
        assert_eq!(prepared.transformation.checkout, base.checkout);
        assert_eq!(
            prepared.transformation.diff_base,
            Some(transformation.checkout),
            "the inverted range is the candidate check's closure, not the workspace"
        );
        assert_ne!(
            prepared.transformation.diff_base,
            Some(prepared.transformation.checkout),
            "base..base would empty the closure"
        );
        scoped += 1;
    }
    assert!(scoped >= 1, "attribution issued a baseline probe over the candidate check's closure");
    assert_eq!(harness.bloom(bloom).members.len(), 1);
}
