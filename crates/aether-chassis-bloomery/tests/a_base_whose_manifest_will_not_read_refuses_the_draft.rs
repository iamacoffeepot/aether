#![cfg(all(unix, feature = "github"))]

//! A base carrying a `pipeline.toml` this coordinator cannot read refuses draft
//! formation, naming the base and the failure — it never falls back to the
//! compiled vocabulary (ADR-0215).
//!
//! The fallback is the tempting answer and the wrong one: it is silent at
//! exactly the moment the tree and the coordinator disagree most, and a
//! fallback that works is a fallback nobody removes. The manifest here declares
//! its own network posture, which the reader refuses rather than ignores —
//! confinement is the coordinator's, and a tree that named its own would grant
//! itself egress on the coordinator's host.
//!
//! A base carrying *no* manifest is the other case and is deliberately not this
//! one. The file cannot arrive by the mechanism that requires it, so every base
//! sealed before it landed stays sealable and the missing-file refusal is armed
//! only once every sealable base carries one — the last slice of ADR-0215, not
//! this one. A file that is present and will not decode has no such excuse: it
//! exists because somebody edited it, and the refusal reaches the person
//! holding the diff.

use aether_bloomery::{PIPELINE_MANIFEST_PATH, PipelineManifest};
use aether_data::Kind;
use aether_harness_bloomery::{HarnessBuilder, Repo};
use serde_json::Value;

/// A manifest of the version this coordinator reads that names something only
/// the coordinator may decide.
const CONFINEMENT_CLAIMING: &str = "version = 1\n\
     [entrypoint]\nprogram = \"cargo\"\nargs = [\"xtask\", \"transform\"]\n\
     [lanes]\nmodel = []\nmechanical = []\n\
     [verifiers]\nidentities = [\"verify.fmt\"]\n[verifiers.runs]\n\
     [evidence]\nenvelope = 1\n\
     [network]\negress = true\n";

#[test]
fn a_base_whose_manifest_will_not_read_refuses_the_draft() {
    let authority = Repo::builder()
        .identity("test", "test@example.test")
        .seed_file(PIPELINE_MANIFEST_PATH, CONFINEMENT_CLAIMING)
        .bare_clone()
        .create();
    let mut harness = HarnessBuilder::local_authority(&authority).start("unreadable-manifest-refuses-the-draft");

    let base = harness.view().mainline;
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: Value = serde_json::from_str(&opened).expect("the draft view is JSON");
    let draft = opened["draft_id"].as_str().expect("a draft view names its handle").to_owned();

    let (status, refused) =
        harness.request("PATCH", &format!("/drafts/{draft}"), &format!(r#"{{"base":"{}"}}"#, base.to_hex()));

    assert_eq!(status, 422, "an unreadable manifest refuses draft formation: {refused}");
    assert!(refused.contains(&base.to_hex()), "the refusal names the base it read: {refused}");
    assert!(refused.contains(PIPELINE_MANIFEST_PATH), "the refusal names the file: {refused}");
    assert!(refused.contains("network"), "the refusal names the failure the reader hit: {refused}");

    // The refusal is total: the draft seals no vocabulary at all, so nothing
    // downstream can attest one read off a tree that would not answer.
    let (status, draft) = harness.request("GET", &format!("/drafts/{draft}"), "");
    assert_eq!(status, 200, "the draft survives a refused patch: {draft}");
    let draft: Value = serde_json::from_str(&draft).expect("the draft view is JSON");
    assert_eq!(
        draft["draft"]["configs"]["entries"][PipelineManifest::NAME],
        Value::Null,
        "a refused patch seals no manifest: {draft}",
    );
}
