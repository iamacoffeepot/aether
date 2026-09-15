//! A delta-confirm whose repair cannot have touched the suite carries the
//! suite's earlier verdict, and the member integrates on the whole receipt.
//!
//! ADR-0200's 2026-09-15 amendment lets a re-verify after a Refine skip gates an
//! earlier receipt already proved, on the grounds that the repair's delta could
//! not have moved them. This is the half that has to *work*: the mechanism is a
//! wall-clock optimization, and a ledger that refuses every carry would satisfy
//! every safety test in the repository while buying nothing — the refusal half
//! lives in the sibling scenario, and neither proves anything alone.
//!
//! The claim here is the measured one. Bloom `0f16e207`'s issue-6023 was red
//! only on `verify.suppress`, repaired two comment lines, and re-ran the suite
//! in full; a comment-class delta cannot reach `verify.test`, so the receipt
//! that ran the rest and carried it is a whole receipt and must be admitted as
//! one.

mod common;

use aether_bloomery::{BloomStatus, DeltaClass};
use aether_harness_bloomery::FixtureHarness;

use common::carry::{carrying, to_the_delta_confirm};

#[test]
fn a_reverify_carries_the_gates_its_delta_cannot_reach() {
    let mut harness = FixtureHarness::start("verify-carry-sound");
    let (bloom, first, repaired, confirm) = to_the_delta_confirm(&mut harness);

    assert_eq!(
        confirm.displayed_digest,
        repaired.tree.as_bytes().to_vec(),
        "the delta-confirm judges the repair's capture, not the tree that failed",
    );

    let key = harness.upload_admitted(&carrying(&confirm, first.tree, &[DeltaClass::Comment]));
    assert!(key.starts_with("aether.bloomery.integrate:"), "a sound carry is a receipt like any other: {key}");

    harness.land_the_fold(bloom);

    let landed = harness.bloom(bloom);
    assert_eq!(landed.status, BloomStatus::Landed, "a carried gate carries the bloom to a landing");
    assert_eq!(
        landed.members[0].resolution.as_ref().map(|claim| claim.candidate),
        Some(repaired.tree),
        "the resolution claims the tree the admitted receipt judged",
    );
}
