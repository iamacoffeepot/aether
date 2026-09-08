//! The gate's own decisions, over a real store.

use aether_bloomery::testing::digest;
use aether_bloomery::{
    BloomId, CONSTRUCT_IMPLEMENT_COMMAND, ConfigKind, ConfigRegistry, Event, Fact, ModelProcessInstructions, Nonce,
    REVIEW_CRITIC_COMMAND, SCOPE_FILL_COMMAND, SlotRole, StageCatalog, StageId, Topic, Transformation,
    VERIFY_MEMBER_COMMAND, WorkpieceId,
};
use aether_data::Kind;
use aether_data::wire::{from_bytes, to_vec};
use rusqlite::Error as SqliteError;

use super::{
    ProvenanceRefusal, admit_model_dispatch, authorize_instructions, drain_refusals, gated, journal_refusal,
    pin_instructions, reference_instructions,
};
use crate::bloomery::intake::DispatchRecord;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::{SqliteStore, StoreBackend};

/// A member Construct order under `configs`, carrying a work-order description
/// so the manifest has a context slot to place.
fn record(configs: ConfigRegistry) -> DispatchRecord {
    DispatchRecord {
        nonce: Nonce("dispatch-1".to_owned()),
        bloom: BloomId(digest(1)),
        workpiece: WorkpieceId("wp".to_owned()),
        scope_revision: digest(2),
        candidate: digest(2),
        displayed_digest: digest(2),
        stage: StageId::Construct,
        transformation: Transformation {
            description: Some("do the thing".to_owned()),
            ..Transformation::for_member_stage(
                &StageCatalog::binding_of(StageId::Construct),
                digest(2),
                digest(0xC0),
                digest(0xB0),
            )
        },
        configs,
        profile: StageCatalog::profile_of(StageId::Construct),
    }
}

fn store() -> SqliteStore {
    SqliteStore::open(":memory:").expect("an in-memory store opens")
}

// The gate covers the lanes that run a model and nothing else. A mechanical gate
// has no instructions to ground, and the pre-bloom scoping run has no sealed
// registry a pin could live in — gating either would refuse work that ADR-0214
// does not ask to be refused.
#[test]
fn the_gate_covers_the_model_lanes_a_bloom_seals_for() {
    assert!(gated(CONSTRUCT_IMPLEMENT_COMMAND), "the construct lane runs a model under a bloom's seal");
    assert!(gated(REVIEW_CRITIC_COMMAND), "so does the critic");
    assert!(!gated(VERIFY_MEMBER_COMMAND), "a mechanical gate runs a compiler, not a model");
    assert!(!gated(SCOPE_FILL_COMMAND), "a scoping run precedes the bloom whose registry would carry the pin");
}

// The refusal every bloom sealed before ADR-0214 gets: no pin, no dispatch. This
// is the fail-closed default — nothing here falls back to the instructions in the
// checkout, which is the substitution the ADR exists to stop.
#[test]
fn an_unpinned_dispatch_is_refused() {
    let mut store = store();

    assert!(matches!(
        admit_model_dispatch(&mut store, &record(ConfigRegistry::default())),
        Err(ProvenanceRefusal::Unpinned)
    ));
}

// Tripwire: content identity is re-derived from the stored bytes, not trusted.
// `record_config` takes the address from its caller, so a row can be filed at an
// address its bytes do not hash to. Without this check an operator's
// authorization of one bundle's digest would stand for whatever text happened to
// be filed there — the authorization would name a digest and admit a different
// document.
#[test]
fn a_bundle_whose_stored_bytes_address_elsewhere_is_refused() {
    let mut store = store();
    let bundle = reference_instructions();
    let registry = authorize_instructions(&mut store, &bundle);

    let mut swapped = bundle.clone();
    swapped.construct = "substituted construct instructions".to_owned();
    store
        .record_config(
            bundle.address().as_bytes(),
            ModelProcessInstructions::NAME,
            &to_vec(&swapped).expect("the substituted bundle encodes"),
        )
        .expect("the substituted row records");

    assert!(
        matches!(admit_model_dispatch(&mut store, &record(registry)), Err(ProvenanceRefusal::ContentMismatch { .. })),
        "content filed under an address it does not hash to must not ride that address's authorization",
    );
}

// ADR-0214's central claim: knowing a bundle's digest and sealing it is not
// authorizing it. A bloom that pins a complete, resolvable bundle the host never
// authorized is refused, so a member cannot select its own process by sealing one.
#[test]
fn a_pinned_but_unauthorized_bundle_is_refused() {
    let mut store = store();
    let registry = pin_instructions(&mut store, &reference_instructions());

    assert!(matches!(admit_model_dispatch(&mut store, &record(registry)), Err(ProvenanceRefusal::Unauthorized { .. })));
}

// An incomplete bundle is refused even when the host authorized its digest:
// authorization says which document may serve as policy, and completeness says
// the document says anything at all. Neither stands in for the other.
#[test]
fn an_authorized_but_incomplete_bundle_is_refused() {
    let mut store = store();
    let registry = authorize_instructions(
        &mut store,
        &ModelProcessInstructions { review: String::new(), ..reference_instructions() },
    );

    assert!(matches!(admit_model_dispatch(&mut store, &record(registry)), Err(ProvenanceRefusal::Incomplete { .. })));
}

// The admitting case, and the shape of what it admits: the authorized bundle is
// the manifest's one instruction slot, and the dispatched work order rides as
// context. That separation is ADR-0214 §Process policy does not authorize
// arbitrary task text — a task that entered as an instruction slot would be
// asking the assembler to ground operator-authored prose as policy.
#[test]
fn an_authorized_bundle_admits_with_the_task_as_context() {
    let mut store = store();
    let bundle = reference_instructions();
    let registry = authorize_instructions(&mut store, &bundle);

    let manifest = admit_model_dispatch(&mut store, &record(registry)).expect("an authorized bundle admits");
    let instructions: Vec<_> = manifest.slots.iter().filter(|slot| slot.role == SlotRole::Instruction).collect();

    assert_eq!(instructions.len(), 1, "one instruction slot: the process policy");
    assert_eq!(instructions[0].artifact, bundle.address(), "and it is the authorized bundle");
    assert!(
        manifest.slots.iter().any(|slot| slot.role == SlotRole::Context),
        "the work order is present, and present as context",
    );
}

// A refused dispatch reaches the journal rather than only a log line: the parked
// row carries the member host fault the reactor admits on its next tick, which is
// what turns "this member stopped" into "this member stopped, and here is why".
#[test]
fn a_permanent_refusal_parks_a_member_host_fault_to_admit() {
    let mut store = store();
    let record = record(ConfigRegistry::default());

    journal_refusal(&mut store, &record, &ProvenanceRefusal::Unpinned);
    let admits = drain_refusals(&mut store).expect("the parked refusal drains");

    assert_eq!(admits.len(), 1, "one refusal, one admission");
    let event = from_bytes::<Event>(&admits[0].event).expect("the parked event decodes");
    match event.fact {
        Fact::MemberExecutorFault { bloom, workpiece, stage, evidence } => {
            assert_eq!(bloom, record.bloom);
            assert_eq!(workpiece, record.workpiece);
            assert_eq!(stage, record.stage);
            assert_eq!(evidence.subject, record.displayed_digest, "the reducer binds the fault to what was displayed");
        }
        other => panic!("a refused member dispatch is a member host fault: {other:?}"),
    }
    assert!(drain_refusals(&mut store).expect("the second drain reads").is_empty(), "the drain acked what it took");
}

// A transient store fault is not a decision about the content, so it parks
// nothing: the dispatch re-drives, and a refusal nobody reached must not be on
// the journal saying the member's process was unauthorized.
#[test]
fn a_transient_refusal_parks_nothing() {
    let mut store = store();
    let record = record(ConfigRegistry::default());

    journal_refusal(&mut store, &record, &ProvenanceRefusal::Store(SqliteError::QueryReturnedNoRows));

    assert!(store.drain_topic(Topic::RefusedDispatch).expect("the topic reads").is_empty());
}
