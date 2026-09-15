//! A scope run dispatches the seat it named, or the compiled line's when it
//! named none.
//!
//! Before the run carried its own override the payload always bore the
//! compiled Scope seat, so a scope run could not be moved off it without a
//! code change. This scenario opens one run naming a grok seat and one naming
//! nothing on two commissions of one harness, pumps the scope drain, and
//! reads each dispatched order back: the first must fork grok and record its
//! override digest, the second the compiled opus seat and no digest.
//!
//! One test, two commissions on one harness: the fixture backend is
//! process-global, so a second harness in this binary would share its
//! mainline.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use aether_bloomery::{
    AgentProfile, AgentSelection, Digest, Harness, ModelOverride, Observation, Provenance, ReasoningEffort,
    StageCatalog, StageId, StageOverride, Statement, Transformation, WorkpieceId,
};
use aether_chassis_bloomery::bloomery::open_scope_run_with_override;
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, ScenarioHarness};

const SEATED: &str = "wp-scope-seat";
const PLAIN: &str = "wp-scope-plain";

fn grok_override() -> ModelOverride {
    ModelOverride {
        per_stage: BTreeMap::from([(
            StageId::Scope,
            StageOverride {
                agent: Some(AgentSelection { harness: Harness::Grok, model: String::from("grok-4.6") }),
                reasoning_effort: Some(ReasoningEffort::High),
            },
        )]),
        ..ModelOverride::default()
    }
}

fn open_commission(harness: &ScenarioHarness, id: &str) -> Digest {
    let mut store = harness.commission_store();
    let intent = Statement {
        words: format!("scope {id}").into_bytes(),
        provenance: Provenance::ObservationAttestation(Observation { source: "scope-seat scenario".to_owned() }),
        parents: Vec::new(),
    };
    store.create(&WorkpieceId(id.to_owned()), &intent).expect("the commission is created")
}

fn await_scope_order(harness: &mut ScenarioHarness, commission: &str) -> OutstandingOrder {
    harness.pump_until("the scope drain dispatches the run", |harness| {
        harness.orders().iter().any(|order| order.workpiece == commission)
    });
    harness
        .orders()
        .into_iter()
        .find(|order| order.workpiece == commission)
        .expect("the pumped drain dispatched the run")
}

fn seat_of(order: &OutstandingOrder) -> (AgentProfile, Transformation) {
    let profile = from_bytes::<AgentProfile>(&order.profile).expect("the order carries its seat profile");
    let transformation =
        from_bytes::<Transformation>(&order.transformation).expect("the order carries its transformation");
    (profile, transformation)
}

#[test]
fn scope_runs_dispatch_their_named_seat_or_the_compiled_line() {
    let roots = HarnessRoots::create();
    let mut harness = HarnessBuilder::fixture().roots(&roots).start("scope-run-seats");
    let base = harness.view().mainline;

    let seated_intent = open_commission(&harness, SEATED);
    let plain_intent = open_commission(&harness, PLAIN);
    let digest = Digest::from_bytes([0x51; 32]);
    {
        let mut store = harness.commission_store();
        open_scope_run_with_override(
            &mut store,
            &WorkpieceId(SEATED.to_owned()),
            seated_intent,
            base,
            "scope sketch",
            &grok_override(),
            Some(digest),
        )
        .expect("the seated run opens");
        open_scope_run_with_override(
            &mut store,
            &WorkpieceId(PLAIN.to_owned()),
            plain_intent,
            base,
            "scope sketch",
            &ModelOverride::default(),
            None,
        )
        .expect("the seatless run opens");
    }

    let order = await_scope_order(&mut harness, SEATED);
    let (profile, transformation) = seat_of(&order);
    assert_eq!(profile.harness, Harness::Grok, "the seated run forks the named harness");
    assert_eq!(profile.model, "grok-4.6", "under the named model");
    assert_eq!(profile.effort, ReasoningEffort::High, "at the named effort");
    let dispatched = transformation.model.expect("a model lane names its seat");
    assert_eq!(dispatched.harness, Harness::Grok, "the order shows the named harness");
    assert_eq!(dispatched.model, "grok-4.6", "the order shows the named model");
    assert_eq!(dispatched.effort, ReasoningEffort::High, "the order shows the named effort");

    let rows = harness.commission_store().list_scope_runs(SEATED).expect("the run ledger reads");
    let enqueued = rows.iter().find(|row| row.kind == "enqueued").expect("the run enqueued");
    assert_eq!(
        enqueued.model_override.as_deref(),
        Some(digest.as_bytes().as_slice()),
        "the run records the digest its seat resolved from"
    );

    let order = await_scope_order(&mut harness, PLAIN);
    let (profile, transformation) = seat_of(&order);
    let compiled = StageCatalog::profile_of(StageId::Scope);
    assert_eq!(profile, compiled, "no override dispatches the line");
    let dispatched = transformation.model.expect("a model lane names its seat");
    assert_eq!(dispatched.harness, compiled.harness, "the order shows the line's harness");
    assert_eq!(dispatched.model, compiled.model, "the order shows the line's model");
    assert_eq!(dispatched.effort, compiled.effort, "the order shows the line's effort");

    let rows = harness.commission_store().list_scope_runs(PLAIN).expect("the run ledger reads");
    let enqueued = rows.iter().find(|row| row.kind == "enqueued").expect("the run enqueued");
    assert!(enqueued.model_override.is_none(), "a seatless run records no digest");
}
