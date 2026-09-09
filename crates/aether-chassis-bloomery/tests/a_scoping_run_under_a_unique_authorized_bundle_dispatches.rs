#![cfg(all(unix, feature = "github"))]

//! A scoping run under a host that authorized exactly one instruction bundle
//! dispatches with that bundle as the manifest's sole instruction slot
//! (ADR-0214).
//!
//! The drain copies the run's pin into the order's registry, and the same
//! provenance gate construct uses assembles the prompt. This scenario asserts
//! on that assembled manifest — what the dispatched lane is authorized to run
//! under — not on a real model.

use aether_bloomery::{
    BloomId, ConfigKind, ConfigRegistry, Digest, ModelProcessInstructions, Nonce, Observation, Provenance, SlotRole,
    StageId, Statement, Transformation, WorkpieceId,
};
use aether_chassis_bloomery::bloomery::{DispatchRecord, admit_model_dispatch, open_scope_run, reference_instructions};
use aether_chassis_bloomery::store::{CommissionBackend, OutstandingOrder, StoreBackend};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, Lane, LaneScript};

#[test]
fn a_scoping_run_under_a_unique_authorized_bundle_dispatches_that_bundle() {
    let roots = HarnessRoots::create();
    let mut harness = HarnessBuilder::fixture()
        .lane_axis(Lane::Scripted)
        .script(&LaneScript::all_passing())
        .roots(&roots)
        .start("pinned-scope-run");

    let expected = reference_instructions().address();
    let commission = WorkpieceId("wp-scope".to_owned());
    let intent = Statement {
        words: b"scope this workpiece".to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: "scenario".to_owned() }),
        parents: Vec::new(),
    };
    let mut store = harness.commission_store();
    let intent_digest = store.create(&commission, &intent).expect("the commission is created");
    let base = harness.view().mainline;
    open_scope_run(&mut store, &commission, intent_digest, base, "scope sketch")
        .expect("the run opens with the host pin");
    drop(store);

    let orders = harness.await_orders(1);
    assert_eq!(orders[0].workpiece, commission.0, "the scoping run is the outstanding order");

    let configs: ConfigRegistry = from_bytes(&orders[0].configs).expect("the order carries a registry");
    assert_eq!(
        configs.address::<ModelProcessInstructions>(),
        Some(expected),
        "the drain handed the lane the host's unique authorized bundle",
    );

    let mut store = harness.commission_store();
    let manifest = admit_model_dispatch(&mut store, &scope_record(&orders[0], configs))
        .expect("the dispatched order's pin admits");
    let instructions: Vec<_> = manifest.slots.iter().filter(|slot| slot.role == SlotRole::Instruction).collect();
    assert_eq!(instructions.len(), 1, "one instruction slot: the process policy");
    assert_eq!(instructions[0].artifact, expected, "and it is the run's pinned bundle");
}

fn scope_record(order: &OutstandingOrder, configs: ConfigRegistry) -> DispatchRecord {
    let subject = Digest::from_slice(&order.displayed_digest).expect("a recorded order displays a whole digest");
    DispatchRecord {
        nonce: Nonce(order.nonce.clone()),
        bloom: BloomId(Digest::from_slice(&order.bloom).expect("a recorded order names a bloom")),
        workpiece: WorkpieceId(order.workpiece.clone()),
        scope_revision: subject,
        candidate: subject,
        displayed_digest: subject,
        stage: StageId::Scope,
        transformation: from_bytes::<Transformation>(&order.transformation)
            .expect("the order carries a transformation"),
        configs,
        profile: from_bytes(&order.profile).expect("the order carries a profile"),
    }
}
