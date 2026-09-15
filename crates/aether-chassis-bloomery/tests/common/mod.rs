//! Shared fixtures for the cross-process tests that fork the `bloomery`
//! coordinator bin: a free localhost port, and a guard that owns a forked
//! coordinator for the life of a binding.
//!
//! Scenario harnesses live in [`aether_harness_bloomery`]. This module re-exports
//! the pieces remaining binaries still need after the promotion (issue 5332),
//! and holds the setup two scenario binaries share where a `FixtureHarness` is
//! process-global and so cannot host both of a pair in one binary.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot set up its process reports it by panicking")]

pub use aether_harness_bloomery::client;
pub use aether_harness_bloomery::{Coordinator, Ingress, free_port};

fn _every_binary_names_the_fixtures() {
    let _: fn() -> u16 = free_port;
    let _: fn(u16, &[(&str, &str)]) -> Coordinator = Coordinator::spawn;
    let _ = Ingress::Rpc;
    let _ = client::connect_and_handshake;
}

/// Driving one member to its delta-confirm, and the coverage claims a
/// re-verify can make there (ADR-0200's 2026-09-15 amendment).
///
/// Shared because the two answers the ledger gives a claim — take it, refuse it
/// — need the same six steps in front of them and a `FixtureHarness` is
/// process-global, so they cannot be two `#[test]`s of one binary.
pub mod carry {
    use core::iter::once;

    use aether_bloomery::{
        BloomId, CandidateRef, CarriedCoverage, CarriedGate, DeltaClass, Digest, VerifyFailure, VerifyFailureSet,
    };
    use aether_chassis_bloomery::bloomery::{ScriptedUpload, ScriptedVerdict};
    use aether_chassis_bloomery::store::OutstandingOrder;
    use aether_harness_bloomery::{FixtureHarness, captured, digest, passed, verdict};

    /// The workpiece the single sealed member covers.
    pub const WORKPIECE: &str = "wp";

    /// The gate the first Verify fails on — the one the refine lap repairs, and
    /// so the one no later receipt may carry.
    fn suppress() -> VerifyFailureSet {
        once(VerifyFailure::Suppress).collect()
    }

    fn verify_failed(order: &OutstandingOrder) -> ScriptedUpload {
        ScriptedUpload { failed_verifiers: suppress(), ..verdict(order, ScriptedVerdict::VerificationFailed) }
    }

    /// A passing re-verify claiming it carried `verify.test` from a receipt over
    /// `proved`, on a delta the lane read as `classes`.
    ///
    /// `verify.test` in both directions on purpose: it is the longest gate in
    /// the umbrella and so the one worth carrying, and the one a wrong table row
    /// would most expensively excuse.
    pub fn carrying(order: &OutstandingOrder, proved: Digest, classes: &[DeltaClass]) -> ScriptedUpload {
        let receipt = digest(0xE1);
        let carried =
            vec![CarriedGate { gate: VerifyFailure::Test.as_str().to_owned(), receipt, tree: proved, passed: true }];

        ScriptedUpload {
            carried: Some(CarriedCoverage { proved, receipt, classes: classes.to_vec(), carried }),
            ..passed(order)
        }
    }

    /// Drive one member to its delta-confirm: construct, a Verify red on
    /// `verify.suppress`, and a refine lap that captures a repaired tree.
    ///
    /// Returns the bloom, the tree the failing Verify judged — what a carry
    /// names as `proved` — the repaired tree the delta-confirm stands on, and
    /// that confirm's order.
    pub fn to_the_delta_confirm(
        harness: &mut FixtureHarness,
    ) -> (BloomId, CandidateRef, CandidateRef, OutstandingOrder) {
        let bloom = harness.seal_member(WORKPIECE, digest(0x51));

        let construct = harness.await_order();
        let first = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xD1));
        harness.upload_admitted(&captured(&construct, first));

        let verify = harness.await_order();
        harness.upload_admitted(&verify_failed(&verify));

        let refine = harness.await_order();
        let repaired = harness.seed_capture(bloom, WORKPIECE, digest(0xC2), digest(0xD2));
        harness.upload_admitted(&captured(&refine, repaired));

        (bloom, first, repaired, harness.await_order())
    }
}
