//! A named test failure's baseline probe must rerun the candidate check's
//! closure, not the whole workspace.
//!
//! Pre-fix: `probe_transformation` rewrote the empty-subset probe to
//! `verify.base` and cleared `--diff-base`, so attribution re-proved every
//! crate to answer one named test. The inverted range (candidate checkout as
//! `--diff-base`, composition base as HEAD) is the same reverse-dependency
//! closure the candidate check already ran.
//!
//! The composition is two members because a bisection is only ever bought for
//! one (#6054): a run with a single member is attributed from its findings and
//! issues no probe at all, so a single-member fixture could not reach the
//! preparation this scenario is about.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::slice::from_ref;

use aether_bloomery::{
    CONSTRUCT_IMPLEMENT_COMMAND, CandidateRef, CoordinationPolicy, Digest, FakeKeyProvider, KeyId, SharedRunDispatch,
    SharedRunExecution, SharedRunMode, Transformation, VERIFY_CHECK_COMMAND, VerificationMode, signed_approval,
};
use aether_chassis_bloomery::bloomery::BatchProbeRequest;
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneMode, LaneScript};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
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
        let approval = signed_approval(KeyId(String::from("baseline-closure harness")), &[0x0A; 32], *scope_revision);
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
    // Unkeyed steps: both constructs park so each member writes its own file,
    // then the first contextual `verify.check` fails. The canned findings name
    // `crates/mock/src/lib.rs`, which no member changed, so the gate does not
    // discriminate and the walk buys the baseline this scenario inspects.
    let script = LaneScript::all_passing()
        .then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits)
        .then(CONSTRUCT_IMPLEMENT_COMMAND, LaneMode::NeverExits)
        .then(VERIFY_CHECK_COMMAND, LaneMode::Fail);
    let mut harness = HarnessBuilder::local_authority(&authority)
        .coordination(contextual_policy())
        .max_concurrent_provers(2)
        .script(&script)
        .start("baseline-probe-closure");
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

    let mut inspected = 0;
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
        inspected += 1;
    }
    assert!(inspected >= 1, "attribution issued a baseline probe over the candidate check's closure");
    assert_eq!(harness.bloom(bloom).members.len(), 2);
}
