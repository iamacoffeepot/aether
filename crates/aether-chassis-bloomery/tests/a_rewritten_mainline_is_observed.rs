//! A history rewrite of the mainline ref recovers by observation (#4938).
//!
//! The host used to classify an unrelated live head as
//! `ObserveMainlineDiverged` and record nothing, so after a force-push
//! both `mainline` and `observed` stayed pinned to a commit the remote no
//! longer has. New blooms sealed on that vanished base and landed with
//! `BaseMismatch`. This drives the rewrite through the live observer.

use aether_harness_bloomery::{FixtureHarness, digest};

/// A force-pushed tip with no ancestry from the coordinator's current
/// mainline. Recognizable, and no bloom of this coordinator's ever names it.
const REWRITTEN: u8 = 0x7D;

#[test]
fn a_rewritten_mainline_recovers_by_observation() {
    let mut harness = FixtureHarness::start("a-rewritten-mainline-is-observed");

    let booted = harness.view().mainline;
    let rewritten = digest(REWRITTEN);
    assert_ne!(booted, rewritten, "the rewritten head has to be one the boot observation did not already read");

    harness.rewrite_mainline(rewritten);
    harness.await_mainline(rewritten);
}
