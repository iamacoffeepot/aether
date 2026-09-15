//! What this command's own code decides, as distinct from what the coordinator
//! decides: the stage spelling an operator types, which findings `--all` is
//! about to void, and which bloom's nonces `status` is looking at.

use aether_bloomery::{BloomStatus, CompositionFinding, CompositionView, Digest, ReviewParkView, StageId, WorkpieceId};

use super::super::dto::{BloomView, LiveOrderView, test_bloom, test_member};
use super::{addresses_workpiece, bloom_orders, open_findings, parse_stage, stage_name};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn bloom() -> BloomView {
    test_bloom(digest(1), BloomStatus::Sealed, Vec::new())
}

fn finding(detail: u8) -> CompositionFinding {
    CompositionFinding { subject: digest(0x70), detail: digest(detail), implicated: Vec::new() }
}

fn order(nonce: &str, bloom: Digest, workpiece: &str) -> LiveOrderView {
    LiveOrderView { nonce: nonce.to_owned(), bloom, workpiece: workpiece.to_owned(), stage: StageId::AggregateReview }
}

#[test]
fn every_stage_the_vocabulary_has_is_spellable_and_parses_back() {
    // Tripwire: the rendered spelling is computed from the variant name, so a
    // stage the vocabulary gains is spellable without an edit here — and a
    // change to how the dash is placed would silently make `--stage
    // aggregate-review` unspellable while every single-word stage kept working.
    for stage in StageId::ALL.iter().copied() {
        let spelled = stage_name(stage);
        assert_eq!(parse_stage(&spelled), Ok(stage), "`{spelled}` must parse back to the stage that produced it");
    }
    assert_eq!(stage_name(StageId::AggregateReview), "aggregate-review");
    assert_eq!(stage_name(StageId::Verify), "verify");
}

#[test]
fn an_unknown_stage_is_refused_with_the_whole_vocabulary() {
    let refusal = parse_stage("aggregate_review").expect_err("an underscore is not the spelling");
    assert!(refusal.contains("aggregate-review"), "the refusal lists what is spellable: {refusal}");
}

#[test]
fn waiving_everything_reaches_both_channels_a_finding_can_be_open_on() {
    // The plausible bug, and the one that matters: a bloom parked at its review
    // ceiling carries its verdict on `review_park` rather than on the findings
    // channel, so a `--all` that read only the composition would void nothing
    // and leave the bloom exactly as stuck as it was.
    let parked = BloomView {
        composition: Some(CompositionView { cursor: None, wedge: None, findings: vec![finding(0x41), finding(0x42)] }),
        review_park: Some(ReviewParkView {
            question: digest(0x43),
            stage: None,
            prompt: None,
            options: Vec::new(),
            blocked: None,
        }),
        ..bloom()
    };

    assert_eq!(open_findings(&parked), vec![digest(0x41), digest(0x42), digest(0x43)]);
}

#[test]
fn a_bloom_with_nothing_open_offers_nothing_to_void() {
    // `--all` bails on an empty set rather than posting a waiver that names no
    // finding: the coordinator would refuse it, and the operator would be told
    // about their request rather than about their bloom.
    assert!(open_findings(&bloom()).is_empty());
}

#[test]
fn status_reads_only_the_named_blooms_lanes() {
    // The plausible bug: `/view` carries every live order in the fleet,
    // including bloom-less whole-workspace stages. A status that listed them
    // all would hand the operator a nonce belonging to another bloom, and
    // `cancel-lane` takes whatever nonce it is given.
    let mine = digest(1);
    let theirs = digest(2);
    let orders = [order("dispatch-1", mine, "wp-a"), order("dispatch-2", theirs, "wp-b"), order("base-1", theirs, "")];

    let listed: Vec<&str> =
        bloom_orders(&orders, &mine.to_hex()).into_iter().map(|order| order.nonce.as_str()).collect();
    assert_eq!(listed, vec!["dispatch-1"]);
}

#[test]
fn the_composition_is_addressable_without_being_a_member() {
    // The live bug this check exists to not repeat: every act takes
    // `<workpiece|composition>`, and the read-first check walks the membership
    // list — where the composition never appears. The sibling `repair` and
    // `retry` verbs refuse it today despite their help text accepting it, which
    // is exactly the workpiece an operator repairing a red aggregate review is
    // aiming at.
    let bloom = BloomView { members: vec![test_member("wp-a", digest(7))], ..bloom() };

    assert!(addresses_workpiece(&bloom, WorkpieceId::COMPOSITION));
    assert!(addresses_workpiece(&bloom, "wp-a"));
    assert!(!addresses_workpiece(&bloom, "wp-typo"));
}
