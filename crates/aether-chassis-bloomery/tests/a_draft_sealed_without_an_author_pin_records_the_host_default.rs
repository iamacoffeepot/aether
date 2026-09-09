#![cfg(all(unix, feature = "github"))]

//! A draft sealed without an author-supplied instruction pin records the host
//! default pin in the sealed bloom's registry (ADR-0214 §Resolve defaults
//! before sealing).
//!
//! The reducer stays pure: the host injects the unique authorized bundle into
//! the draft's registry before the spec is frozen, the same way draft formation
//! injects the pipeline manifest derived from the base. A bloom that sealed
//! no pin of its own still names an explicit bundle, so later dispatch does
//! not consult whatever default happens to be installed then.

use aether_bloomery::{
    ApprovalPolicy, ConfigKind, ConfigRegistry, Digest, Event, Fact, FakeKeyProvider, ModelProcessInstructions,
    Observation, PIPELINE_MANIFEST_PATH, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, Statement,
    Tier, WorkpieceId, decode_recorded_event,
};
use aether_chassis_bloomery::bloomery::reference_instructions;
use aether_chassis_bloomery::commission::task_text;
use aether_chassis_bloomery::store::{CommissionBackend, RevisionEvidence, StoreBackend};
use aether_data::Kind;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness, member};
use serde_json::Value;

const CHECKED_IN: &str = include_str!("../../../pipeline.toml");
const WORKPIECE: &str = "wp-pin";

#[test]
fn a_draft_sealed_without_an_author_pin_records_the_host_default() {
    let authority = Repo::builder()
        .identity("test", "test@example.test")
        .seed_file(PIPELINE_MANIFEST_PATH, CHECKED_IN)
        .bare_clone()
        .create();
    let mut harness = HarnessBuilder::local_authority(&authority).start("seal-default-instruction-pin");

    let expected = reference_instructions().address();
    let revision = seed_complete_revision(&harness, WORKPIECE, &["docs/guide/**"]);

    let policy = ApprovalPolicy { default: Tier::Auto, rules: Vec::new() };
    let (status, stored) =
        harness.post("/configs", &serde_json::json!({ "kind": ApprovalPolicy::NAME, "value": policy }).to_string());
    assert_eq!(status, 200, "the auto-tier policy authors: {stored}");

    let mut configs = ConfigRegistry::default();
    configs.insert::<ApprovalPolicy>(policy.address());
    let base = harness.view().mainline;
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: Value = serde_json::from_str(&opened).expect("the draft view is JSON");
    let draft = opened["draft_id"].as_str().expect("a draft view names its handle");

    let patch = serde_json::json!({
        "base": base.to_hex(),
        "proposals": [member(WORKPIECE, revision)],
        "configs": configs,
    });
    let (status, patched) = harness.request("PATCH", &format!("/drafts/{draft}"), &patch.to_string());
    assert_eq!(status, 200, "the draft takes a base and a member: {patched}");
    let patched: Value = serde_json::from_str(&patched).expect("the draft view is JSON");
    assert!(
        patched["draft"]["configs"]["entries"][ModelProcessInstructions::NAME].is_null(),
        "the author supplied no instruction pin: {patched}",
    );

    let (status, sealed) = harness.post(&format!("/drafts/{draft}/seal"), "{}");
    assert_eq!(status, 200, "an auto-tier member seals: {sealed}");

    let pin = sealed_instruction_pin(&mut harness);
    assert_eq!(pin, expected, "the sealed bloom records the host's unique authorized bundle");
}

fn seed_complete_revision(harness: &ScenarioHarness, workpiece: &str, surface: &[&str]) -> Digest {
    let mut store = harness.commission_store();
    let workpiece = WorkpieceId(workpiece.to_owned());
    let intent = Statement {
        words: format!("scope {}", workpiece.0).into_bytes(),
        provenance: Provenance::ObservationAttestation(Observation { source: "scenario".to_owned() }),
        parents: Vec::new(),
    };
    store.create(&workpiece, &intent).expect("the commission is created");
    let revision = ScopeRevision {
        schema: SCOPE_REVISION_SCHEMA,
        workpiece,
        predecessor: None,
        problem: "the harness authored this scope".to_owned(),
        design: "design notes so the gate's completeness check admits".to_owned(),
        plan: "plan so the gate's completeness check admits".to_owned(),
        declared_surface: surface.iter().map(|glob| (*glob).to_owned()).collect(),
        dogfood_brief: "dogfood".to_owned(),
        routing: ScopeRouting { size: "S".to_owned(), model: String::new() },
        dependencies: Vec::new(),
        description: String::new(),
        implements: Vec::new(),
        declared_crates: Vec::new(),
        declared_reads: Vec::new(),
    };
    let revision = ScopeRevision { description: task_text(&revision), ..revision };
    let digest = store.write_revision(&revision, &RevisionEvidence::default()).expect("the complete revision writes");
    store.insert_approval(&auto_approval(digest), &FakeKeyProvider).expect("the auto-tier approval stores");
    digest
}

fn auto_approval(scope: Digest) -> Statement {
    Statement {
        words: scope.as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(Observation {
            source: "aether.bloomery.approve_gate:auto-tier".to_owned(),
        }),
        parents: vec![scope],
    }
}

fn sealed_instruction_pin(harness: &mut ScenarioHarness) -> Digest {
    harness
        .commission_store()
        .replay_journal()
        .expect("the journal replays")
        .iter()
        .filter_map(|record| decode_recorded_event(&record.event, record.event_schema.as_deref()).ok())
        .find_map(|event: Event| match event.fact {
            Fact::Seal(spec) => spec.configs().address::<ModelProcessInstructions>(),
            _ => None,
        })
        .expect("an admitted bloom records the instruction pin it sealed")
}
