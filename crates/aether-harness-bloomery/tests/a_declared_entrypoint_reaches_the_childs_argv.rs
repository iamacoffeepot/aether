//! A manifest declaring a different `[entrypoint]` reaches the child's argv
//! (ADR-0215).
//!
//! The host override is empty, so the dispatch reads the sealed entrypoint
//! rather than `AETHER_BLOOMERY_LANE_PROGRAM`. The program is still the mock
//! lane: the test is about which words precede the work order, not about
//! running `cargo xtask transform`.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{BloomDraft, Fact, Outcome, PIPELINE_MANIFEST_PATH, PipelineManifest, config_address};
use aether_data::Kind;
use aether_data::wire::to_vec;
use aether_harness_bloomery::{HarnessBuilder, Repo, digest, member, mock_lane_program};
use serde_json::Value;

const CHECKED_IN: &str = include_str!("../../../pipeline.toml");
const VIA_MANIFEST: &str = "via-manifest";

#[test]
fn a_declared_entrypoint_reaches_the_childs_argv() {
    let program = mock_lane_program();
    let text = with_mock_entrypoint(&program);
    let authority = Repo::builder()
        .identity("test", "test@example.test")
        .seed_file(PIPELINE_MANIFEST_PATH, text.as_str())
        .bare_clone()
        .create();
    let mut harness =
        HarnessBuilder::local_authority(&authority).lane_from_manifest().start("declared-entrypoint-reaches-argv");

    let base = harness.view().mainline;
    let derived = derived_manifest(&harness, base, &text);
    let template = harness.successor_draft(&[member("wp", digest(0x51))]);
    let mut configs = template.configs().clone();
    configs.insert::<PipelineManifest>(derived);
    let spec =
        BloomDraft { proposals: template.members().to_vec(), base, configs, forecast: template.forecast() }.seal();

    match harness.admit("seal-a-declared-entrypoint", Fact::Seal(spec)) {
        Outcome::Sealed(_) => {}
        other => panic!("a base that declares its lanes must seal, got {other:?}"),
    }

    harness.pump_until("a lane dispatch records the sealed entrypoint", |harness| {
        harness.ledger().iter().any(|run| run.argv.iter().any(|word| word == VIA_MANIFEST))
    });

    let argv = harness
        .ledger()
        .into_iter()
        .find(|run| run.argv.iter().any(|word| word == VIA_MANIFEST))
        .expect("pump_until saw the word")
        .argv;
    assert!(
        argv.iter().any(|word| word == VIA_MANIFEST),
        "the sealed extra entrypoint word reaches the child: {argv:?}"
    );
    assert!(
        argv.iter().any(|word| word.contains("construct") || word.contains("verify") || word.contains("review")),
        "the work-order command still follows the sealed leading words: {argv:?}"
    );
}

fn with_mock_entrypoint(program: &str) -> String {
    let escaped = program.replace('\\', "\\\\").replace('"', "\\\"");
    let from = "[entrypoint]\nprogram = \"cargo\"\nargs = [\"xtask\", \"transform\"]";
    let to = format!("[entrypoint]\nprogram = \"{escaped}\"\nargs = [\"{VIA_MANIFEST}\"]");
    let text = CHECKED_IN.replace(from, &to);
    assert!(text.contains(&escaped), "the fixture must actually rewrite the entrypoint");
    let manifest = PipelineManifest::from_toml(&text).expect("a manifest with a mock entrypoint still reads");
    assert_eq!(manifest.entrypoint.program, program);
    assert_eq!(manifest.entrypoint.args, [VIA_MANIFEST]);
    text
}

fn derived_manifest(
    harness: &aether_harness_bloomery::ScenarioHarness,
    base: aether_bloomery::Digest,
    text: &str,
) -> aether_bloomery::Digest {
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: Value = serde_json::from_str(&opened).expect("the draft view is JSON");
    let draft = opened["draft_id"].as_str().expect("a draft view names its handle");

    let (status, patched) =
        harness.request("PATCH", &format!("/drafts/{draft}"), &format!(r#"{{"base":"{}"}}"#, base.to_hex()));
    assert_eq!(status, 200, "naming the base derives its declaration: {patched}");

    let bytes = to_vec(&PipelineManifest::from_toml(text).expect("the fixture reads")).expect("a manifest encodes");
    config_address(PipelineManifest::NAME, &bytes)
}
