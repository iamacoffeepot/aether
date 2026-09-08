//! A base that carries no `pipeline.toml` is refused at draft formation, and a
//! `Fact::Seal` that names no manifest is refused at the seal door (ADR-0215).
//!
//! The compiled vocabulary is not a fallback for a fresh seal: a fallback is
//! silent at the exact moment the tree and the coordinator disagree most. The
//! draft-formation 422 names the base and the path so the operator holding the
//! tree can add the file; the seal-door variant is the same rule for a spec
//! admitted without going through a draft. A successor of a pre-manifest bloom
//! goes through that door too, so it must be sealed against a base that carries
//! the file. Records already journaled keep folding against `compiled()`.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomDraft, Fact, Outcome, PIPELINE_MANIFEST_PATH, SealError};
use aether_harness_bloomery::{HarnessBuilder, Repo, digest, member};
use serde_json::Value;

#[test]
fn a_manifestless_base_is_refused() {
    let authority =
        Repo::builder().identity("test", "test@example.test").omit_pipeline_manifest().bare_clone().create();
    let mut harness = HarnessBuilder::local_authority(&authority).start("manifestless-base-is-refused");

    let base = harness.view().mainline;
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: Value = serde_json::from_str(&opened).expect("the draft view is JSON");
    let draft = opened["draft_id"].as_str().expect("a draft view names its handle").to_owned();

    let (status, refused) =
        harness.request("PATCH", &format!("/drafts/{draft}"), &format!(r#"{{"base":"{}"}}"#, base.to_hex()));
    assert_eq!(status, 422, "a missing file refuses draft formation: {refused}");
    assert!(refused.contains(&base.to_hex()), "the refusal names the base: {refused}");
    assert!(refused.contains(PIPELINE_MANIFEST_PATH), "the refusal names the path: {refused}");
    assert!(refused.contains("a base must declare its lanes"), "the refusal states the rule: {refused}");

    let spec = BloomDraft { proposals: vec![member("wp", digest(0x51))], base, ..BloomDraft::default() }.seal();
    match harness.admit("seal-without-a-manifest", Fact::Seal(spec)) {
        Outcome::SealRejected(SealError::UnusablePipelineManifest { base: named, path }) => {
            assert_eq!(named, base, "the refusal names the spec's base");
            assert_eq!(path, PIPELINE_MANIFEST_PATH, "and the path that base had to carry");
        }
        other => panic!("a spec naming no manifest must refuse the seal, got {other:?}"),
    }
}
