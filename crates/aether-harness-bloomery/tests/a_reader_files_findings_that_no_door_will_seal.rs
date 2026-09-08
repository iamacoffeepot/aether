//! A landed bloom's reader files what it will not fix, and nothing it filed is
//! work until a person makes it so (ADR-0216 §3).
//!
//! Three claims, and none of them is visible from inside the reducer or from a
//! unit test over the value vocabulary.
//!
//! The first is that a lane result reaches the *commission* store at all. A
//! filing is keyed by workpiece and the journal is keyed by bloom, so the write
//! rides the evidence admission rather than a `Fact` — and the only place that
//! seam is real is here, where an order the coordinator actually dispatched is
//! actually answered and the store the operator reads is the store that was
//! written.
//!
//! The second is that what lands is inert at the doors an operator would meet.
//! `verify_authority`, the approval classifier, and the seal gate each refuse a
//! stage-receipt filing for their own reason; asserting the provenance value
//! would restate a `match` arm, so each refusal is taken from the real door —
//! the store's own `insert_approval`, and the running coordinator's
//! `POST /drafts/{id}/seal`.
//!
//! The third is that a malformed emission files nothing. The reader's whole
//! input is a landed diff and its whole output is a proposal that work be done,
//! so the failure worth catching is the one where a garbled payload still puts
//! machine-authored work orders in the estate. A second bloom's read emits a
//! finding with no title, and the count from the first bloom must not move.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    AuthorityDoor, BloomId, CommissionStatus, Digest, FakeKeyProvider, LandingReceipt, Provenance, RetrospectFinding,
    StageId, Statement, digest_of,
};
use aether_chassis_bloomery::store::{CommissionBackend, CommissionError, CommissionHead};
use aether_data::wire::from_bytes;
use aether_harness_bloomery::{FixtureHarness, captured, digest, member, passed, read};
use serde_json::Value;

const FIRST: &str = "wp-0";
const SECOND: &str = "wp-1";

#[test]
fn a_reader_files_findings_that_no_door_will_seal() {
    let mut harness = FixtureHarness::start("reader-files-findings");
    let base = harness.view().mainline;
    let bloom = harness.seal_member(FIRST, digest(0x51));
    walk_to_landing(&mut harness, bloom, FIRST, 0xC1);
    let landed = harness.view().mainline;

    let study = harness.await_order();
    assert_eq!(from_bytes::<StageId>(&study.stage).unwrap(), StageId::Study);
    harness.upload_admitted(&read(
        &study,
        &[
            (
                "the executor drain re-reads a parked entry every tick",
                "The study drain acks past a parked entry, so the next tick re-selects it.",
                &["crates/aether-chassis-bloomery/**"],
            ),
            (
                "a refused emission logs no bloom id",
                "The refusal warn names the nonce and not the bloom, so it cannot be traced back.",
                &["crates/aether-bloomery/**"],
            ),
        ],
    ));

    let filed = open_commissions(&mut harness);
    assert_eq!(filed.len(), 2, "one open commission per finding: {filed:?}");
    for head in &filed {
        assert!(head.id.0.starts_with(RetrospectFinding::ID_PREFIX), "a filing is greppable as one: {head:?}");
        assert!(
            head.current_revision.is_none(),
            "a filing carries no scope revision, so there is no tip a seal could name: {head:?}",
        );
    }

    // The derivation: every filing names the same read, and that read names the
    // receipt it was dispatched over and the range it was dispatched across. A
    // filing whose inputs named anything else would be a work order derived
    // from a bloom the reader never opened.
    let receipt = LandingReceipt { bloom, previous_base: base, new_head: landed }.digest();
    let intents: Vec<Statement> = filed.iter().map(|head| intent_of(&mut harness, head.intent)).collect();
    let mut parents = Vec::new();
    for intent in &intents {
        let Provenance::StageReceipt(stage_receipt) = &intent.provenance else {
            panic!("a filing's intent is grounded in the read that produced it: {intent:?}");
        };
        assert_eq!(stage_receipt.stage, StageId::Study);
        assert_eq!(stage_receipt.inputs, [receipt, base, landed], "the receipt it read, then the range it read");
        assert_eq!(stage_receipt.outputs.len(), 2, "and both filings it produced");
        assert_eq!(intent.parents.len(), 1, "one derivation parent: {intent:?}");
        parents.push(intent.parents[0]);
    }
    assert_eq!(parents[0], parents[1], "both filings came out of the one read");

    // Door one: the statement is not authority, whatever verifies it. The fake
    // provider accepts every message it is given, so a filing that verified
    // here would verify against a real key provider's signature check too.
    for intent in &intents {
        assert!(
            !intent.verify_authority(&FakeKeyProvider, AuthorityDoor::Approve, digest_of(intent)),
            "a filed finding must not verify as its own approval",
        );
    }

    // Door two: the commission store's approval classifier, reached through the
    // real backend rather than restated. Twice, because the filing is refused
    // before the classifier is even asked — its words are a work order, not a
    // scope digest — and the interesting refusal is the one left when that is
    // repaired: the same provenance, re-worded onto a real scope digest, is
    // still not an approval.
    let mut store = harness.commission_store();
    assert_eq!(
        store.insert_approval(&intents[0], &FakeKeyProvider).unwrap_err(),
        CommissionError::WrongSubject,
        "a filing's own words approve nothing: they are the work order, not a revision",
    );
    let reworded = Statement { words: digest(0x77).as_bytes().to_vec(), ..intents[0].clone() };
    assert_eq!(
        store.insert_approval(&reworded, &FakeKeyProvider).unwrap_err(),
        CommissionError::WrongProvenance,
        "and the read's provenance is refused as an approval however it is worded",
    );

    // Door three: the running coordinator's seal gate. A filing has no current
    // revision, so no draft membership can name one that is current.
    let (status, refused) = attempt_seal(&harness, &filed[0]);
    assert_eq!(status, 422, "a filing cannot be sealed: {refused}");
    assert!(
        refused["error"].as_str().unwrap_or_default().contains("stale scope revision"),
        "and is refused for having no revision at all, not for some other reason: {refused}",
    );

    // A second read whose emission is malformed files nothing, and the study is
    // still recorded rather than pretended away.
    let second = harness.seal_member(SECOND, digest(0x52));
    walk_to_landing(&mut harness, second, SECOND, 0xC2);
    let garbled = harness.await_order();
    harness.upload_admitted(&read(
        &garbled,
        &[
            ("a real finding", "with a body and a surface", &["crates/aether-bloomery/**"]),
            ("", "and a sibling with no title at all", &["crates/aether-bloomery/**"]),
        ],
    ));

    assert_eq!(
        open_commissions(&mut harness).len(),
        2,
        "a malformed emission files nothing — not even the entries that were well formed",
    );
    assert!(harness.orders().is_empty(), "the refused emission still consumed its order");
}

/// Carry a freshly sealed single-member bloom through construct, verify, and
/// the fold to its landing.
fn walk_to_landing(harness: &mut FixtureHarness, bloom: BloomId, workpiece: &str, seed: u8) {
    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, workpiece, digest(seed), digest(seed.wrapping_add(0x10)));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));
    harness.land_the_fold(bloom);
}

/// Every open commission the coordinator's store holds, in workpiece-id order.
fn open_commissions(harness: &mut FixtureHarness) -> Vec<CommissionHead> {
    harness.commission_store().list(Some(CommissionStatus::Open)).unwrap()
}

/// One commission's intent statement, read back through the real store.
fn intent_of(harness: &mut FixtureHarness, intent: Digest) -> Statement {
    harness.commission_store().load_statement(intent).unwrap().expect("a filed commission stores its intent")
}

/// Offer `head` as the sole member of a fresh draft and ask the coordinator to
/// seal it, returning the status and the parsed body.
fn attempt_seal(harness: &FixtureHarness, head: &CommissionHead) -> (u16, Value) {
    let (status, opened) = harness.post("/drafts", "null");
    assert_eq!(status, 201, "a draft opens: {opened}");
    let opened: Value = serde_json::from_str(&opened).unwrap();
    let draft = opened["draft_id"].as_str().unwrap().to_owned();

    // Proposals only. Naming a base would send the patch through the ADR-0215
    // manifest derivation, which is a different refusal than the one under
    // test; the seal loads every member's commission before it reads a base at
    // all, so the draft never needs one.
    let patch = serde_json::json!({
        "proposals": [serde_json::to_value(member(&head.id.0, digest(0x99))).unwrap()],
    });
    let (status, patched) = harness.request("PATCH", &format!("/drafts/{draft}"), &patch.to_string());
    assert_eq!(status, 200, "the draft takes the filing as a proposed member: {patched}");

    let (status, refused) = harness.post(&format!("/drafts/{draft}/seal"), "{}");
    (status, serde_json::from_str(&refused).unwrap_or(Value::Null))
}
