//! A committed `DispatchLand` wakes the land reactor without a timer tick.
//!
//! The control core used to write the land outbox row and then wait for the
//! reactor's independent poll — or a restart, which remounts the reactor and
//! fires its boot tick. A row that sat across several configured cadences
//! only moved once the otherwise-idle coordinator was restarted. This
//! scenario never calls [`FixtureHarness::land_tick`] and uses the fixture's
//! day-long cadence, so a proposal that opens can only have come from the
//! post-commit nudge.

mod common;
pub mod fixture;
pub mod harness;

use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::BloomId;
use fixture::{FixtureHarness, captured, digest, passed};

/// The workpiece the single sealed member covers.
const WORKPIECE: &str = "wp";

/// How long the committed land decision may take to open a proposal.
/// Matches the fixture step budget: one in-process drain, not a poll wait.
const PROPOSAL_BUDGET: Duration = Duration::from_secs(20);

#[test]
fn a_committed_land_decision_nudges_the_reactor() {
    let mut harness = FixtureHarness::start("committed-land-decision-nudges-the-reactor");
    let bloom = harness.seal_member(WORKPIECE, digest(0x51));

    let construct = harness.await_order();
    let candidate = harness.seed_capture(bloom, WORKPIECE, digest(0xC1), digest(0xC2));
    harness.upload_admitted(&captured(&construct, candidate));

    let verify = harness.await_order();
    harness.upload_admitted(&passed(&verify));

    // The claim set is complete. Drive the fold and its aggregate gates the
    // same way every other scenario does, then stop: the next wake the land
    // reactor is owed is the one the committed decision must send.
    harness.integrate_tick();

    let order = harness.await_order();
    assert!(order.workpiece.is_empty(), "a bloom-level order carries no member axis");
    let mut key = harness.upload_admitted(&passed(&order));

    let mechanical_ran = key.starts_with("aether.bloomery.aggregate_verify:");
    if mechanical_ran {
        let aggregate_review = harness.await_order();
        key = harness.upload_admitted(&passed(&aggregate_review));
    }
    assert!(key.starts_with("aether.bloomery.aggregate_review:"), "the critic's gate: {key}");

    let proposal = await_landing_proposal(&mut harness, bloom);
    assert!(proposal > 0, "the land reactor opened a numbered proposal");
    assert!(harness.landing_merged(proposal), "the nudge also accepts the proposal it opened");
}

/// Wait until the boot-constructed land reactor opens a proposal for `bloom`.
///
/// No `land_tick` — a wake here would be the scenario driving the reactor,
/// not the committed decision doing it.
fn await_landing_proposal(harness: &mut FixtureHarness, bloom: BloomId) -> u64 {
    let deadline = Instant::now() + PROPOSAL_BUDGET;
    loop {
        if let Some(proposal) = harness.landing_proposal(bloom) {
            return proposal;
        }
        assert!(
            Instant::now() < deadline,
            "the committed land decision never opened a proposal; the reactor was not nudged"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
