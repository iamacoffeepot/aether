#![cfg(all(unix, feature = "github"))]

//! A scoping run under a host that authorized exactly one instruction bundle
//! pins that bundle on the durable run record, and the same provenance gate
//! construct uses assembles it as the manifest's sole instruction slot
//! (ADR-0214).
//!
//! The GitHub actions backend refuses a model lane with no resolved model, so
//! this scenario does not wait on an outstanding order. What the mock lane
//! would be handed is the assembled manifest: the drain copies the run's pin
//! into the order's registry, and `admit_model_dispatch` is that assembly.
//! The capturing-backend unit test next to the drain is what sees the order
//! itself.

use aether_bloomery::{
    BloomId, ConfigKind, ConfigRegistry, Digest, ModelProcessInstructions, Nonce, Observation, Provenance, SlotRole,
    StageCatalog, StageId, Statement, Transformation, WorkpieceId,
};
use aether_chassis_bloomery::bloomery::{DispatchRecord, admit_model_dispatch, open_scope_run, reference_instructions};
use aether_chassis_bloomery::store::{CommissionBackend, StoreBackend};
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots};

#[test]
fn a_scoping_run_under_a_unique_authorized_bundle_dispatches_that_bundle() {
    let roots = HarnessRoots::create();
    let mut harness = HarnessBuilder::fixture().roots(&roots).start("pinned-scope-run");

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

    let rows = store.list_scope_runs(&commission.0).expect("the run ledger reads");
    let pin = rows
        .first()
        .and_then(|row| row.instructions.as_deref())
        .and_then(Digest::from_slice)
        .expect("the host's unique authorized bundle is pinned on the run");
    assert_eq!(pin, expected, "exactly one authorized address is the default pin");

    let mut configs = ConfigRegistry::default();
    configs.insert::<ModelProcessInstructions>(pin);
    let subject = rows[0].subject.as_deref().and_then(Digest::from_slice).expect("an enqueued run names its subject");
    let manifest = admit_model_dispatch(&mut store, &scope_record(commission, subject, base, configs))
        .expect("the run's pin admits");
    let instructions: Vec<_> = manifest.slots.iter().filter(|slot| slot.role == SlotRole::Instruction).collect();
    assert_eq!(instructions.len(), 1, "one instruction slot: the process policy");
    assert_eq!(instructions[0].artifact, expected, "and it is the run's pinned bundle");
}

fn scope_record(workpiece: WorkpieceId, subject: Digest, checkout: Digest, configs: ConfigRegistry) -> DispatchRecord {
    DispatchRecord {
        nonce: Nonce("dispatch-scope".to_owned()),
        bloom: BloomId(Digest::from_bytes([0x51; 32])),
        workpiece,
        scope_revision: subject,
        candidate: subject,
        displayed_digest: subject,
        stage: StageId::Scope,
        transformation: Transformation::for_scoping_run(&StageCatalog::binding_of(StageId::Scope), subject, checkout),
        configs,
        profile: StageCatalog::profile_of(StageId::Scope),
    }
}
