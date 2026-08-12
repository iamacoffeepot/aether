//! A member whose Verify fails, repairs, fails the same way again, repairs
//! again, and then passes — and the bloom lands.
//!
//! The failure/repair re-entry is a loop between two reactors that nothing else
//! exercises end to end: the reducer routes a failing Verify to `Refine` and
//! decides a fresh dispatch, and only the executor reactor draining that
//! decision turns it back into a running attempt. A re-entry that decided
//! nothing — or decided something the drain cannot submit — reads as a bloom
//! that simply stops, with a member sitting at a stage nobody is working on.
//!
//! Two failures over the same verifier is the case with the most room to be
//! wrong, because it is where the repair-roll accounting actually moves. A
//! *novel* failure set costs no roll (nothing was repeated), so the first
//! failure re-enters Refine with the count untouched; the second repeats
//! `verify.clippy`, spends the first roll, and is still well inside the sealed
//! budget of three. A miscounted roll wedges a member that had rolls left, and
//! a wedged member is terminal — the bloom can never resolve.

mod common;
mod fixture;

use core::iter::once;

use aether_bloomery::{BloomStatus, VerifyFailure, VerifyFailureSet};
use aether_chassis_bloomery::bloomery::{ScriptedUpload, ScriptedVerdict};
use aether_chassis_bloomery::store::OutstandingOrder;
use fixture::{FixtureHarness, captured, digest, passed, verdict};

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

/// The verifier that fails both times. The same identity twice is what makes
/// the second failure a repeat rather than a fresh one.
fn clippy() -> VerifyFailureSet {
    once(VerifyFailure::Clippy).collect()
}

/// A failing member Verify naming the exact verifiers that failed. The set must
/// be nonempty — the intake refuses a failing Verify that names none, the same
/// contract that refuses any other stage that names some (ADR-0178).
fn verify_failed(order: &OutstandingOrder, failed: VerifyFailureSet) -> ScriptedUpload {
    ScriptedUpload { failed_verifiers: failed, ..verdict(order, ScriptedVerdict::VerificationFailed) }
}

#[test]
fn a_bloom_that_fails_verification_twice_still_lands() {
    let mut harness = FixtureHarness::start("verify-repair-scenario");
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    let construct = harness.await_order();
    let first = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xD1));
    harness.upload_admitted(&captured(&construct, first));

    // The first failure names a verifier the member has never failed on, so it
    // re-enters Refine having spent no repair roll.
    let verify = harness.await_order();
    let key = harness.upload_admitted(&verify_failed(&verify, clippy()));
    assert!(key.starts_with("aether.bloomery.verify_failed:"), "a failing Verify carries its typed identities: {key}");

    // The repair. It captures a different tree — a repair that changed nothing
    // would be a repair in name only — and the reducer returns the member to
    // Verify against it.
    let refine = harness.await_order();
    let repaired = harness.seed_capture(bloom, WORKPIECE, digest(0xC2), digest(0xD2));
    harness.upload_admitted(&captured(&refine, repaired));

    let verify = harness.await_order();
    assert_eq!(
        verify.displayed_digest,
        repaired.tree.as_bytes().to_vec(),
        "the re-entered Verify runs against the repair's capture, not the one that failed",
    );

    // The second failure repeats `verify.clippy`, so it spends the first repair
    // roll — one of three the sealed catalog allows. A member wedged here would
    // never dispatch again.
    harness.upload_admitted(&verify_failed(&verify, clippy()));
    assert!(
        harness.bloom(bloom).members[0].wedge.is_none(),
        "a repeated failure inside the repair budget re-opens the member rather than stopping it",
    );

    let refine = harness.await_order();
    let final_capture = harness.seed_capture(bloom, WORKPIECE, digest(0xC3), digest(0xD3));
    harness.upload_admitted(&captured(&refine, final_capture));

    let verify = harness.await_order();
    let key = harness.upload_admitted(&passed(&verify));
    assert!(key.starts_with("aether.bloomery.integrate:"), "the third Verify passes and integrates: {key}");

    harness.land_the_fold(bloom);

    let landed = harness.bloom(bloom);
    assert_eq!(landed.status, BloomStatus::Landed, "a member that repaired twice still carries its bloom to a landing");
    assert_eq!(
        landed.members[0].resolution.as_ref().map(|claim| claim.candidate),
        Some(final_capture.tree),
        "the resolution claims the tree the passing Verify bound, not one of the trees that failed",
    );
}
