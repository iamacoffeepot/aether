#![cfg(all(unix, feature = "github"))]

//! A draft formed against a base that declares its lanes seals that
//! declaration, and no operator can author the same entry by hand (ADR-0215).
//!
//! The lane vocabulary the coordinator dispatches against is a property of the
//! tree being worked, not of the binary doing the dispatching — which is why
//! holding it compiled produced five copies of one list and three of them
//! drifted. So the repository states it once in `pipeline.toml`, and the
//! coordinator reads that statement out of the base it is about to seal
//! against.
//!
//! Two halves, and they only mean something together. The host derives the
//! registry entry from the base's own blob, so the digest a bloom attests is
//! the vocabulary its checkout carries. And `POST /configs` refuses the kind,
//! so there is no second way in: the reducer cannot fetch, and the authoring
//! route sees no base at all, so an authored entry would let a draft attest a
//! vocabulary nothing checked against a tree — the attested-but-untrue
//! divergence ADR-0174 exists to remove.
//!
//! The address is taken over the *decoded* manifest rather than the file text,
//! which is what this scenario pins by computing it from the checked-in file
//! through the same decode: reformatting `pipeline.toml` re-seals nothing, and
//! only a change in meaning moves a digest that is about to appear in every
//! receipt.

use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{PIPELINE_MANIFEST_PATH, PipelineManifest, config_address};
use aether_data::Kind;
use aether_data::wire::to_vec;
use aether_harness_bloomery::{HarnessBuilder, Repo, ScenarioHarness};
use serde_json::Value;

/// This repository's own manifest — the text the base carries.
const CHECKED_IN: &str = include_str!("../../../pipeline.toml");

/// How long the boot configuration read has to land before a read of the
/// address the patch handed back is a failure rather than a race.
const READ_BUDGET: Duration = Duration::from_secs(10);

#[test]
fn a_draft_seals_the_manifest_its_base_declares() {
    let authority = Repo::builder()
        .identity("test", "test@example.test")
        .seed_file(PIPELINE_MANIFEST_PATH, CHECKED_IN)
        .bare_clone()
        .create();
    let mut harness = HarnessBuilder::local_authority(&authority).start("manifest-from-the-sealed-base");

    let base = harness.view().mainline;
    let draft = open_draft(&harness);
    let (status, patched) =
        harness.request("PATCH", &format!("/drafts/{draft}"), &format!(r#"{{"base":"{}"}}"#, base.to_hex()));
    assert_eq!(status, 200, "naming a base that declares its lanes shapes the draft: {patched}");

    let bytes = to_vec(&PipelineManifest::from_toml(CHECKED_IN).expect("the checked-in manifest reads"))
        .expect("a manifest encodes");
    let address = config_address(PipelineManifest::NAME, &bytes);
    let patched: Value = serde_json::from_str(&patched).expect("the draft view is JSON");
    assert_eq!(
        patched["draft"]["configs"]["entries"][PipelineManifest::NAME],
        Value::String(address.to_hex()),
        "the draft seals the address of the value its base declares: {patched}",
    );

    // ADR-0174's rule that a sealed address resolves: the content is filed
    // before the patch answers, so the address it hands back is readable
    // rather than a promise about a row somebody still has to write.
    let stored = read_config(&harness, &address.to_hex());
    assert_eq!(stored["kind"], PipelineManifest::NAME, "the stored row is filed under the manifest kind: {stored}");
    assert_eq!(stored["value"]["version"], 1, "the stored bytes decode as the manifest the base declared: {stored}");

    // The other half. An operator holding the same value cannot put it in a
    // registry themselves, because nothing here could check it against a tree.
    let (status, refused) = harness
        .post("/configs", &serde_json::json!({ "kind": PipelineManifest::NAME, "value": stored["value"] }).to_string());
    assert_eq!(status, 422, "the manifest kind is derived, never authored: {refused}");
    assert!(
        refused.contains(PIPELINE_MANIFEST_PATH),
        "the refusal names the file the entry is derived from instead: {refused}",
    );
}

/// Open a draft and answer its handle.
fn open_draft(harness: &ScenarioHarness) -> String {
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: Value = serde_json::from_str(&opened).expect("the draft view is JSON");
    opened["draft_id"].as_str().expect("a draft view names its handle").to_owned()
}

/// Read one stored configuration back, waiting out the boot read a cold cap
/// answers `503` for.
fn read_config(harness: &ScenarioHarness, address: &str) -> Value {
    let deadline = Instant::now() + READ_BUDGET;
    loop {
        let (status, body) = harness.request("GET", &format!("/configs/{address}"), "");
        if status == 200 {
            return serde_json::from_str(&body).expect("a stored configuration is JSON");
        }
        assert!(status == 503, "reading the sealed manifest answered {status}: {body}");
        assert!(Instant::now() < deadline, "the boot configuration read never landed");
        thread::sleep(Duration::from_millis(100));
    }
}
