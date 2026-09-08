#![cfg(all(unix, feature = "github"))]

//! A verify verdict naming a verifier identity the bloom's base does not
//! declare is refused at admission, and its order is not spent (ADR-0215).
//!
//! This is the strict half of the decode-tolerant / intake-strict move, end to
//! end. The lane names an identity; nothing between the lane and this door holds
//! a manifest, so nothing between them can judge it — the decoder that reads the
//! evidence deliberately admits any well-formed identity so a row from a
//! coordinator with a wider vocabulary still folds. The admission door is where
//! the bloom is in hand, and therefore where the vocabulary the bloom sealed is
//! in hand.
//!
//! What a silent admission would cost is ADR-0178's forgiveness bound. A member
//! spends no repair roll on a verdict whose identities it has not failed on
//! before, so an identity outside the sealed vocabulary is one more free lap —
//! and an unbounded supply of them is an unbounded repair loop, which is the
//! property the closed enum existed to guarantee and the sealed vocabulary now
//! guarantees in its place.
//!
//! The observables are the journal and the order, not a log line: no
//! `Fact::VerifyFailed` is ever admitted, and the refusal precedes the consume,
//! so the order the honest verdict would have answered is still live rather
//! than spent on a verdict nobody could read.

use aether_bloomery::{
    BloomDraft, BloomId, ConfigRegistry, Decision, Decisions, Digest, Event, Fact, ModelProcessInstructions, Outcome,
    PIPELINE_MANIFEST_PATH, PipelineManifest, StageId, VerifyFailure, VerifyFailureSet, WorkpieceId,
};
use aether_chassis_bloomery::store::{OutstandingOrder, SqliteStore, StoreBackend};
use aether_data::Kind;
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, LaneScript, Repo, ScenarioHarness, digest, member};
use serde_json::Value;

/// This repository's own manifest — the text the base would carry.
const CHECKED_IN: &str = include_str!("../../../pipeline.toml");

const WORKPIECE: &str = "wp";

/// The base's manifest: this repository's vocabulary less the identity appended
/// most recently (#5309), so the tenth is well-formed, compiled, and undeclared
/// all at once — which is the case a compiled-table membership test cannot tell
/// apart from a declared one.
fn nine_identities() -> String {
    CHECKED_IN
        .lines()
        .map(|line| line.replace("\"verify.lock\",", "").replace(", \"verify.lock\"", ""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_verdict_naming_an_undeclared_verifier_is_refused() {
    let declared = PipelineManifest::from_toml(&nine_identities()).expect("the nine-identity manifest reads");
    assert_eq!(declared.identity_count(), 9, "the fixture must actually drop an identity");
    assert!(!declared.declares_verifier(VerifyFailure::Lock));

    let authority = Repo::builder()
        .identity("test", "test@example.test")
        .seed_file(PIPELINE_MANIFEST_PATH, nine_identities().as_str())
        .bare_clone()
        .create();
    let roots = HarnessRoots::create();
    let mut harness = HarnessBuilder::local_authority(&authority).roots(&roots).start("undeclared-verifier");

    // The lane names the identity the base dropped. Every other position keeps
    // the all-passing script, so the run reaches Verify on its own.
    harness.script_lane(
        &WorkpieceId(WORKPIECE.to_owned()),
        StageId::Verify,
        &[LaneScript::VerifyFail(VerifyFailureSet::one(VerifyFailure::Lock))],
    );

    let base = harness.view().mainline;
    // ADR-0214: an unpinned bloom cannot start a model attempt, so the draft
    // pins the harness's instruction bundle exactly as `seal_member` does. This
    // scenario has to reach Verify, which means Construct has to dispatch.
    let mut configs = ConfigRegistry::default();
    configs.insert::<ModelProcessInstructions>(harness.instructions());
    configs.insert::<PipelineManifest>(derive_manifest(&harness, base));
    let spec =
        BloomDraft { proposals: vec![member(WORKPIECE, digest(0x51))], base, configs, ..BloomDraft::default() }.seal();
    let bloom = spec.id();
    assert!(
        matches!(harness.admit("seal-nine-identities", Fact::Seal(spec)), Outcome::Sealed(id) if id == bloom),
        "a bloom sealing the address its base declared is admitted",
    );

    // Run until the lane has actually rendered its verdict, then give the intake
    // cycle every chance to admit it.
    harness.run_until(verify_ran, 80);
    for _ in 0..5 {
        harness.tick();
    }

    assert_eq!(
        recorded_identities(&roots.store_path(), bloom),
        declared.identities().map(String::from).collect::<Vec<_>>(),
        "the bloom is judged against the vocabulary its base declared",
    );
    let admitted = journaled_verify_failures(&roots.store_path());
    assert!(
        admitted.is_empty(),
        "a verdict naming an identity the base does not declare never reaches the reducer, got {admitted:?}",
    );
    assert!(
        harness.orders().iter().any(|order| stage_of(order) == StageId::Verify),
        "the refusal precedes the consume, so the order the honest verdict would answer is still live",
    );

    let view = harness.bloom(bloom);
    let member = &view.members[0];
    assert!(member.wedge.is_none(), "an unreadable verdict is not a member defect: {member:?}");
    assert!(member.resolution.is_none(), "and it certainly does not resolve one: {member:?}");
}

/// Whether the scripted lane has run this member's Verify position.
fn verify_ran(harness: &mut ScenarioHarness) -> bool {
    harness.ledger().iter().any(|run| run.stage == Some(StageId::Verify) && run.workpiece.as_deref() == Some(WORKPIECE))
}

/// The stage one outstanding order dispatched.
fn stage_of(order: &OutstandingOrder) -> StageId {
    from_bytes(&order.stage).expect("a recorded order carries a StageId")
}

/// Every verifier set a journaled member-verify failure carries — the fact an
/// admitted verdict would have become, and what it claimed.
fn journaled_verify_failures(store_path: &str) -> Vec<VerifyFailureSet> {
    let mut store = SqliteStore::open(store_path).expect("the journal opens for reading");
    store
        .replay_journal()
        .expect("the journal replays")
        .into_iter()
        .filter_map(|record| from_bytes::<Event>(&record.event).ok())
        .filter_map(|event| match event.fact {
            Fact::VerifyFailed { failed_verifiers, .. } => Some(failed_verifiers),
            _ => None,
        })
        .collect()
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

/// The verifier vocabulary `bloom`'s journal rows record.
fn recorded_identities(store_path: &str, bloom: BloomId) -> Vec<String> {
    let mut store = SqliteStore::open(store_path).expect("the journal opens for reading");
    store
        .replay_journal()
        .expect("the journal replays")
        .into_iter()
        .filter_map(|record| from_bytes::<Decisions>(&record.decisions).ok())
        .flat_map(|decisions| decisions.effects)
        .find_map(|effect| match effect {
            Decision::RecordPipelineManifest { bloom: recorded, manifest } if recorded == bloom => {
                Some(manifest.identities().map(String::from).collect())
            }
            _ => None,
        })
        .expect("an admitted bloom records the vocabulary it runs")
}
