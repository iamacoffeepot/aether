//! A member whose Verify comes back red leaves the bloom, no repair lap is
//! ever dispatched, and the sibling that stayed carries the bloom to a landing
//! (ADR-0218 §Amendment: low tolerance, 2026-09-15).
//!
//! The reducer's own cases prove the decision; what only the whole loop can
//! prove is that the decision *closes*. An ejection touches three reactors that
//! never had to agree before: the control core decides a withdrawal set where
//! it used to decide a repair dispatch, the executor reactor must find nothing
//! in the outbox to submit for the member that left, and the integrate reactor
//! must fold a claim set that is complete because one of its members is gone
//! rather than because every member resolved. A bloom that ejects and then
//! quietly stops — a fold nobody dispatches, a member sitting at a stage nobody
//! works on — reads exactly like a bloom that is still working.
//!
//! The sibling is what makes the ejection visible as a *decision* rather than
//! as a bloom failing: if the ejecting member were the only one, "the bloom
//! stopped" and "the bloom ejected and finished" would produce the same
//! terminal status.

use core::iter::once;

use aether_bloomery::{BloomStatus, VerifyFailure, VerifyFailureSet};
use aether_chassis_bloomery::bloomery::{ScriptedUpload, ScriptedVerdict};
use aether_chassis_bloomery::store::OutstandingOrder;
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed, verdict};

/// The member whose Verify comes back red, and the one that stays.
const EJECTED: &str = "wp-red";
const SURVIVOR: &str = "wp-green";

/// A failing member Verify naming the verifier that failed and the prose the
/// lane wrote. Both ride the ejection's reason, which is the only thing a
/// person picking the candidate up gets to read.
fn verify_failed(order: &OutstandingOrder, findings: &str) -> ScriptedUpload {
    ScriptedUpload {
        failed_verifiers: once(VerifyFailure::Clippy).collect::<VerifyFailureSet>(),
        findings: Some(findings.to_owned()),
        ..verdict(order, ScriptedVerdict::VerificationFailed)
    }
}

#[test]
fn a_red_member_verify_ejects_it_and_its_sibling_still_lands() {
    let mut harness = FixtureHarness::start("red-verify-ejects-scenario");
    let bloom = harness.seal_members(&[(EJECTED, digest(0x51)), (SURVIVOR, digest(0x52))]);

    let constructs = harness.await_orders(2);
    for order in &constructs {
        let candidate = harness.seed_capture(bloom, &order.workpiece, digest(0xC1), digest(0xD1));
        harness.upload_admitted(&captured(order, candidate));
    }

    // Both members reach Verify. One passes and integrates; the other comes
    // back red and is withdrawn on the spot.
    let verifies = harness.await_orders(2);
    for order in &verifies {
        if order.workpiece == SURVIVOR {
            harness.upload_admitted(&passed(order));
        } else {
            let key = harness.upload_admitted(&verify_failed(order, "wp-red/src/lib.rs:12 needless_borrow"));
            assert!(key.starts_with("aether.bloomery.verify_failed:"), "the red verdict is still a verify fact: {key}");
        }
    }

    let view = harness.bloom(bloom);
    let ejected = view.members.iter().find(|member| member.workpiece.0 == EJECTED).expect("the red member is listed");
    let departure = ejected.withdrawn.as_ref().expect("the red member left the bloom");
    assert_eq!(departure.cause, "verify");
    assert!(departure.reason.contains("verify.clippy"), "the reason names the gate: {}", departure.reason);
    assert!(departure.reason.contains("needless_borrow"), "and what it said: {}", departure.reason);
    assert!(ejected.cursor.is_none(), "an ejected member sits at no stage");
    assert!(ejected.wedge.is_none(), "an ejection is not a wedge — no budget was spent");

    // The load-bearing negative: nothing is owed to the member that left. A
    // Refine order here would be the repair lap the amendment exists to stop,
    // and it would also mean the ejection did not actually close the line.
    assert!(harness.orders().is_empty(), "the ejection dispatches nothing: {:?}", harness.orders());

    harness.land_the_fold(bloom);

    let landed = harness.bloom(bloom);
    assert_eq!(landed.status, BloomStatus::Landed, "the sibling that stayed carries the bloom to a landing");
    let survivor =
        landed.members.iter().find(|member| member.workpiece.0 == SURVIVOR).expect("the green member is listed");
    assert!(survivor.resolution.is_some(), "the member that passed resolves");
    assert!(
        landed
            .members
            .iter()
            .find(|member| member.workpiece.0 == EJECTED)
            .is_some_and(|member| member.resolution.is_none()),
        "and the ejected one contributes no claim to the fold",
    );
}
