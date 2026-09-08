//! A bloom sealed against a base whose `pipeline.toml` drops an identity from
//! `verify.member`'s run dispatches a member verify whose gate set lacks it
//! (ADR-0215 slice 6).
//!
//! The per-position fan-out is data now: dropping `verify.dup` from the member
//! run — an identity the compiled member still names — must move the gate-set
//! digest the proof is filed under. A dispatch that still keyed on
//! `VerifyGateSet::member()` would file under the compiled identity and the
//! memo would answer for a gate that never ran.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomDraft, Decision, Digest, Fact, Outcome, PIPELINE_MANIFEST_PATH, PipelineManifest, StageId, VerifyFailure,
    VerifyGateSet, VerifyProof, config_address, decode_recorded_decisions,
};
use aether_chassis_bloomery::store::StoreBackend;
use aether_data::Kind;
use aether_data::wire::{from_bytes, to_vec};
use aether_harness_bloomery::{HarnessBuilder, Lane, Repo, ScenarioHarness, captured, digest, member, passed};

/// This repository's own manifest — the text a base normally carries.
const CHECKED_IN: &str = include_str!("../../../pipeline.toml");

const MEMBER: &str = "wp";

#[test]
fn a_member_verify_runs_the_sealed_manifests_gates() {
    let authority = Repo::builder()
        .identity("test", "test@example.test")
        .seed_file(PIPELINE_MANIFEST_PATH, without_dup_on_member())
        .bare_clone()
        .create();
    let mut harness =
        HarnessBuilder::local_authority(&authority).lane_axis(Lane::Off).start("member-verify-reads-manifest");

    let base = harness.view().mainline;
    let manifest = PipelineManifest::from_toml(&without_dup_on_member()).expect("the fixture reads");
    let template = harness.successor_draft(&[member(MEMBER, digest(0x51))]);
    let mut configs = template.configs().clone();
    configs.insert::<PipelineManifest>(derived_manifest(&harness, base));
    let spec = BloomDraft {
        proposals: template.members().to_vec(),
        base,
        configs,
        forecast: template.forecast(),
        ..BloomDraft::default()
    }
    .seal();
    let bloom = spec.id();

    match harness.admit("seal-against-a-base-that-dropped-a-member-gate", Fact::Seal(spec)) {
        Outcome::Sealed(sealed) => assert_eq!(sealed, bloom),
        other => panic!("a base that still declares every lane must seal, got {other:?}"),
    }

    let first = harness.await_order();
    let construct = if from_bytes::<StageId>(&first.stage).unwrap() == StageId::BaseVerify {
        harness.upload_admitted(&passed(&first));
        harness.await_order()
    } else {
        first
    };
    assert_eq!(from_bytes::<StageId>(&construct.stage).unwrap(), StageId::Construct);
    let candidate = harness.seed_capture(bloom, MEMBER, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    assert_eq!(from_bytes::<StageId>(&verify.stage).unwrap(), StageId::Verify);
    harness.upload_admitted(&passed(&verify));

    let expected = VerifyGateSet::member_of(&manifest).digest();
    assert!(
        !VerifyGateSet::member_of(&manifest).verifiers.contains(VerifyFailure::Dup),
        "the sealed member run dropped dup"
    );
    assert_ne!(expected, VerifyGateSet::member().digest(), "that drop must move the identity");

    let proof = filed_member_proof(&harness).expect("a passing member verify files a proof");
    assert_eq!(proof.stage, StageId::Verify);
    assert_eq!(proof.gate_set, expected, "the proof is filed under the sealed member run list, not the compiled one");
}

/// The checked-in manifest with `verify.dup` removed from the member fan-out.
fn without_dup_on_member() -> String {
    let compiled = "\"verify.member\" = [\"verify.preflight\", \"verify.fmt\", \"verify.clippy\", \"verify.test\", \"verify.dup\", \"verify.deps\", \"verify.suppress\", \"verify.lock\"]";
    let dropped = "\"verify.member\" = [\"verify.preflight\", \"verify.fmt\", \"verify.clippy\", \"verify.test\", \"verify.deps\", \"verify.suppress\", \"verify.lock\"]";
    let text = CHECKED_IN.replace(compiled, dropped);
    assert!(text.contains(dropped), "the fixture must actually rewrite the member run");
    let manifest = PipelineManifest::from_toml(&text).expect("a manifest short one member identity still reads");
    assert!(
        !VerifyGateSet::member_of(&manifest).verifiers.contains(VerifyFailure::Dup),
        "the decoded member gate set must omit it too"
    );
    text
}

/// Shape a draft onto `base` so the host reads the file out of that tree and
/// files the decoded bytes, then answer the address those bytes are filed
/// under.
fn derived_manifest(harness: &ScenarioHarness, base: Digest) -> Digest {
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: serde_json::Value = serde_json::from_str(&opened).expect("the draft view is JSON");
    let draft = opened["draft_id"].as_str().expect("a draft view names its handle");

    let (status, patched) =
        harness.request("PATCH", &format!("/drafts/{draft}"), &format!(r#"{{"base":"{}"}}"#, base.to_hex()));
    assert_eq!(status, 200, "naming the base derives its declaration: {patched}");

    let bytes = to_vec(&PipelineManifest::from_toml(&without_dup_on_member()).expect("the fixture reads"))
        .expect("a manifest encodes");
    config_address(PipelineManifest::NAME, &bytes)
}

fn filed_member_proof(harness: &ScenarioHarness) -> Option<VerifyProof> {
    let mut store = harness.commission_store();
    store
        .replay_journal()
        .expect("the journal replays")
        .iter()
        .filter_map(|record| {
            decode_recorded_decisions(&record.decisions, record.decisions_schema_digest.as_deref()).ok()
        })
        .flat_map(|decisions| decisions.effects)
        .find_map(|effect| match effect {
            Decision::RecordVerifyProof { proof, .. } if proof.stage == StageId::Verify => Some(proof),
            _ => None,
        })
}
