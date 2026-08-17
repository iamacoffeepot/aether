//! A resolved bloom reaches `Landed` with nobody pressing the merge button.
//!
//! The landing path used to end on a person: the coordinator opened the landing
//! proposal and then waited for an operator to notice and merge it. That was the
//! only human action left in an otherwise unattended loop, it cost minutes to
//! hours per bloom depending on who was looking, and it was invisible — nothing
//! said "waiting on a merge", so a resolved bloom just sat.
//!
//! Every other landing scenario has [`FixtureHarness::land_the_fold`] take the
//! bloom through its aggregate gates and the land tick. This one asserts that
//! the land reactor itself merges the proposal it opened — nobody presses the
//! button, and check state is not consulted (#5110).

mod common;
pub mod fixture;

use aether_bloomery::BloomStatus;
use fixture::{FixtureHarness, captured, digest, passed};

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

#[test]
fn a_resolved_bloom_lands_on_the_coordinators_own_merge() {
    let mut harness = FixtureHarness::start("coordinator-merges-its-own-landing");
    let sealed_on = harness.view().mainline;
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xC2));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));

    let (_, proposal) = harness.resolve_and_propose(bloom);
    assert!(harness.landing_merged(proposal), "the coordinator merged on the structural gates");

    // Nothing below merges anything — the ticks `await_landing` drives are
    // the land reactor's own poll wake, which observes the merge it already
    // performed. The Admit is detached, so status may still read Resolved
    // until that wake lands.
    harness.await_landing(bloom, BloomStatus::Landed);

    assert!(harness.landing_merged(proposal), "the coordinator merged the landing it opened");
    assert_ne!(harness.view().mainline, sealed_on, "mainline advanced off the base the bloom sealed on");
}
