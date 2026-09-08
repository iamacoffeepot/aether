//! A bloom sealed against a base whose `pipeline.toml` does not declare a lane
//! the catalog dispatches is refused at the door, naming the stage, the lane,
//! and what the base does declare (ADR-0215 slice 4).
//!
//! This is the first refusal the record arms. Before it there was no value to
//! compare a catalog against, so "the tree I checked out cannot run what I am
//! about to dispatch" had no place to be noticed: the seal was admitted, the
//! member walked its line, and the mismatch surfaced as a stage wedged with no
//! attempt ever made — at which point the operator who authored the catalog has
//! moved on and the bloom has already claimed its members.
//!
//! The omitted lane is `review.critic` on purpose. The Review binding's
//! `process` is the host position `review`, so a cross-check reading `process`
//! alone still admits this catalog; only the command the stage's dispatch
//! constructs catches it. And the whole path is real — the host reads the file
//! out of the base's tree at draft formation, files the decoded bytes, and the
//! reducer refuses off the address the spec sealed — because the derivation and
//! the refusal are one claim and neither half means anything alone.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomDraft, CatalogError, ConfigRegistry, Digest, Fact, Outcome, PIPELINE_MANIFEST_PATH, PipelineManifest,
    REVIEW_CRITIC_COMMAND, SealError, StageId, config_address,
};
use aether_data::Kind;
use aether_data::wire::to_vec;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness, digest, member};
use serde_json::Value;

/// This repository's own manifest — the text a base normally carries.
const CHECKED_IN: &str = include_str!("../../../pipeline.toml");

#[test]
fn a_base_that_omits_a_lane_refuses_the_seal() {
    let authority = Repo::builder()
        .identity("test", "test@example.test")
        .seed_file(PIPELINE_MANIFEST_PATH, without_the_review_lane())
        .bare_clone()
        .create();
    let mut harness = HarnessBuilder::local_authority(&authority).start("a-base-missing-a-lane");

    let base = harness.view().mainline;
    let spec = BloomDraft {
        proposals: vec![member("wp", digest(0x51))],
        base,
        configs: sealing(derived_manifest(&harness, base)),
        ..BloomDraft::default()
    }
    .seal();

    match harness.admit("seal-against-a-base-missing-a-lane", Fact::Seal(spec)) {
        Outcome::SealRejected(SealError::CatalogOutsideDeclaredLanes { error, declared }) => {
            assert_eq!(
                error,
                CatalogError::UndeclaredLaneCommand {
                    stage: StageId::Review,
                    command: REVIEW_CRITIC_COMMAND.to_owned(),
                },
                "the refusal names the stage that would have dispatched the lane",
            );
            assert!(
                !declared.iter().any(|command| command == REVIEW_CRITIC_COMMAND),
                "the refusal states the vocabulary the base does carry: {declared:?}",
            );
            assert!(declared.len() > 1, "a base declaring nothing at all would prove less: {declared:?}");
        }
        other => panic!("a base that cannot run the line must refuse the seal, got {other:?}"),
    }
}

/// The checked-in manifest with one model lane removed — a checkout that
/// implements everything the line dispatches except the critic.
fn without_the_review_lane() -> String {
    let text = CHECKED_IN.replace(&format!("\"{REVIEW_CRITIC_COMMAND}\", "), "");
    assert!(!text.contains(REVIEW_CRITIC_COMMAND), "the fixture must actually omit the lane this scenario is about");
    let manifest = PipelineManifest::from_toml(&text).expect("a manifest short one lane still reads");
    assert!(!manifest.declares_lane(REVIEW_CRITIC_COMMAND), "the decoded value must omit it too");
    text
}

/// The registry a spec seals to name that manifest.
fn sealing(address: Digest) -> ConfigRegistry {
    let mut configs = ConfigRegistry::default();
    configs.insert::<PipelineManifest>(address);
    configs
}

/// Shape a draft onto `base` so the host reads the file out of that tree and
/// files the decoded bytes, then answer the address those bytes are filed
/// under. Sealing an address nothing filed would be refused as unproducible
/// long before the cross-check, which would prove nothing about it.
fn derived_manifest(harness: &ScenarioHarness, base: Digest) -> Digest {
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: Value = serde_json::from_str(&opened).expect("the draft view is JSON");
    let draft = opened["draft_id"].as_str().expect("a draft view names its handle");

    let (status, patched) =
        harness.request("PATCH", &format!("/drafts/{draft}"), &format!(r#"{{"base":"{}"}}"#, base.to_hex()));
    assert_eq!(status, 200, "naming the base derives its declaration: {patched}");

    let bytes = to_vec(&PipelineManifest::from_toml(&without_the_review_lane()).expect("the fixture reads"))
        .expect("a manifest encodes");
    config_address(PipelineManifest::NAME, &bytes)
}
