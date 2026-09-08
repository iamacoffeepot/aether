#![cfg(all(unix, feature = "github"))]

//! A bloom sealed against a base that declares its lanes journals that
//! declaration, and a bloom that seals none journals the compiled vocabulary
//! (ADR-0215 slice 3).
//!
//! Under ADR-0190 replay folds recorded decisions rather than re-deciding, so
//! the vocabulary a bloom ran under has to be *in* the journal. Re-reading it
//! from the base at replay would not do: a `pipeline.toml` can be rewritten
//! under a digest no journal row names, and a later binary's compiled copy is
//! exactly the drift this record exists to close — so a bloom would be graded
//! against a vocabulary it never ran, silently.
//!
//! Both doors are here because both resolve it. The seal is the first, and the
//! supersession is the second: a successor promises its own line, so it records
//! its own vocabulary rather than inheriting the predecessor's — and sealing
//! none is what makes the compiled fallback observable end to end, since the
//! value a pre-ADR-0215 bloom folds against is the one it actually ran under.

use aether_bloomery::{
    BloomDraft, BloomId, ConfigRegistry, Decision, Decisions, Digest, Fact, Outcome, PIPELINE_MANIFEST_PATH,
    PipelineManifest, SealError,
};
use aether_chassis_bloomery::store::{SqliteStore, StoreBackend};
use aether_data::Kind;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, Repo, ScenarioHarness, digest, member};
use serde_json::Value;

/// This repository's own manifest — the text the base carries.
const CHECKED_IN: &str = include_str!("../../../pipeline.toml");

const WORKPIECE: &str = "wp";

#[test]
fn a_sealed_bloom_records_the_vocabulary_it_ran() {
    let authority = Repo::builder()
        .identity("test", "test@example.test")
        .seed_file(PIPELINE_MANIFEST_PATH, CHECKED_IN)
        .bare_clone()
        .create();
    let roots = HarnessRoots::create();
    let mut harness = HarnessBuilder::local_authority(&authority).roots(&roots).start("manifest-on-the-record");

    // Shaping a draft against the base is what derives the entry and files its
    // bytes, so the address the spec seals is one the seal door can resolve.
    let base = harness.view().mainline;
    let sealed_address = derive_manifest(&harness, base);

    let mut configs = ConfigRegistry::default();
    configs.insert::<PipelineManifest>(sealed_address);
    let spec =
        BloomDraft { proposals: vec![member(WORKPIECE, digest(0x51))], base, configs, ..BloomDraft::default() }.seal();
    let sealed = spec.id();
    assert!(
        matches!(harness.admit("seal-declaring-base", Fact::Seal(spec)), Outcome::Sealed(id) if id == sealed),
        "a bloom sealing the address its base declared is admitted",
    );

    let declared = PipelineManifest::from_toml(CHECKED_IN).expect("the checked-in manifest reads");
    assert_eq!(
        recorded_manifest(&roots.store_path(), sealed),
        declared,
        "the seal journals the vocabulary its base declared, not the one this binary compiled",
    );

    // The other door. A successor sealing no manifest is refused: supersede
    // goes through the same door as a fresh seal, so a successor of a
    // pre-manifest bloom must be sealed against a base that carries the file.
    let successor =
        BloomDraft { proposals: vec![member(WORKPIECE, digest(0x51))], base, ..BloomDraft::default() }.seal();
    match harness.admit("supersede-without-a-manifest", Fact::Supersede { predecessor: sealed, successor }) {
        Outcome::SupersedeRejected(aether_bloomery::SupersedeError::InvalidMember(
            SealError::UnusablePipelineManifest { path, .. },
        )) => {
            assert_eq!(path, PIPELINE_MANIFEST_PATH, "the refusal names the path the successor's base had to carry");
        }
        other => panic!("a successor naming no manifest must refuse, got {other:?}"),
    }
}

/// Shape a fresh draft onto `base` and answer the manifest address the host
/// derived from that base's tree.
fn derive_manifest(harness: &ScenarioHarness, base: Digest) -> Digest {
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: Value = serde_json::from_str(&opened).expect("the draft view is JSON");
    let draft = opened["draft_id"].as_str().expect("a draft view names its handle").to_owned();

    let (status, patched) =
        harness.request("PATCH", &format!("/drafts/{draft}"), &format!(r#"{{"base":"{}"}}"#, base.to_hex()));
    assert_eq!(status, 200, "naming a base that declares its lanes shapes the draft: {patched}");
    let patched: Value = serde_json::from_str(&patched).expect("the draft view is JSON");
    let hex = patched["draft"]["configs"]["entries"][PipelineManifest::NAME]
        .as_str()
        .unwrap_or_else(|| panic!("the draft seals the manifest its base declares: {patched}"));
    Digest::from_hex(hex).expect("a sealed address is a digest")
}

/// The manifest `bloom`'s journal rows record — read back through a second,
/// non-claiming connection, the way any operator read of the journal is.
fn recorded_manifest(store_path: &str, bloom: BloomId) -> PipelineManifest {
    let mut store = SqliteStore::open(store_path).expect("the journal opens for reading");
    store
        .replay_journal()
        .expect("the journal replays")
        .into_iter()
        .filter_map(|record| from_bytes::<Decisions>(&record.decisions).ok())
        .flat_map(|decisions| decisions.effects)
        .find_map(|effect| match effect {
            Decision::RecordPipelineManifest { bloom: recorded, manifest } if recorded == bloom => Some(manifest),
            _ => None,
        })
        .expect("an admitted bloom records the vocabulary it runs")
}
