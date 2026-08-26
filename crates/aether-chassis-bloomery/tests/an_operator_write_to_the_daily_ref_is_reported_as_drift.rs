//! An operator's fast-forward write to the day's ref is reported as drift,
//! and stays reported after the observer absorbs the new head.
//!
//! Observation records whatever the ref says, so `observed_head_equals_daily_head`
//! goes green once the coordinator has heard of the write. This is the land-
//! grounded row that does not: a descendant of the last land is still not a
//! land. A rewritten tip with no ancestry from that land is someone else's
//! row and must stay silent here.

use aether_bloomery::BloomStatus;
use aether_chassis_bloomery::bloomery::{DoctorReport, Invariant};
use aether_harness_bloomery::{FixtureHarness, captured, digest, passed};

const WORKPIECE: &str = "wp";

/// A fast-forward commit no bloom of this coordinator authored.
const DRIFTED: u8 = 0x7E;

/// A rewritten tip with no ancestry from the last land.
const REWRITTEN: u8 = 0x7D;

#[test]
fn an_operator_write_to_the_daily_ref_is_reported_as_drift() {
    let mut harness = FixtureHarness::start("an-operator-write-to-the-daily-ref-is-reported-as-drift");
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xC2));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));
    harness.land_the_fold(bloom);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed);

    let landed = harness.view().mainline;
    let drifted = digest(DRIFTED);
    assert_ne!(landed, drifted, "the operator's head is not the land");

    harness.move_mainline(drifted);
    harness.doctor_tick();
    assert_last_land_drift(&mut harness, &bloom.0.to_hex(), &landed.to_hex(), &drifted.to_hex());

    harness.await_mainline(drifted);
    harness.doctor_tick();
    assert_last_land_drift(&mut harness, &bloom.0.to_hex(), &landed.to_hex(), &drifted.to_hex());
    let absorbed = doctor(&mut harness);
    let observed =
        absorbed.named(Invariant::ObservedHeadEqualsDailyHead.name()).expect("the seed list includes observed head");
    assert!(observed.passed, "observation of the drifted head is not this row: {observed:?}");

    let rewritten = digest(REWRITTEN);
    harness.rewrite_mainline(rewritten);
    harness.doctor_tick();
    let rewritten_report = doctor(&mut harness);
    let check = rewritten_report
        .named(Invariant::DailyHeadIsCoordinatorLastLand.name())
        .expect("the seed list includes the coordinator's last land");
    assert!(check.passed, "a non-descendant rewrite is not this row: {check:?}");
}

fn doctor(harness: &mut FixtureHarness) -> DoctorReport {
    harness.doctor().expect("the doctor has published a report")
}

fn assert_last_land_drift(harness: &mut FixtureHarness, bloom: &str, landed: &str, drifted: &str) {
    let report = doctor(harness);
    let check = report
        .named(Invariant::DailyHeadIsCoordinatorLastLand.name())
        .expect("the seed list includes the coordinator's last land");
    assert!(!check.passed, "a descendant of the last land is a violation: {check:?}");
    let named = check.divergences.join(" ");
    assert!(named.contains(landed), "the last land's digest is named: {named}");
    assert!(named.contains(drifted), "the live head's digest is named: {named}");
    assert!(named.contains(bloom), "the authoring bloom is named: {named}");
    for forbidden in ["push", "reset", "reconcile"] {
        assert!(!named.contains(forbidden), "the row must not instruct a repair ({forbidden}): {named}");
    }
}
