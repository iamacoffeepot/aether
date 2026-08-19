//! `ObserveTick` is produced on the coordinator poll cadence (#4938).
//!
//! The handler used to exist with no producer, so every observation was
//! the single boot-time one. This starts the timer at one second and
//! waits for a moved head *without* an explicit tick.
//!
//! Its own binary because `GithubConnectionConfig::shared_fixture` is a
//! process-global `OnceLock`: a second `FixtureHarness` in the same
//! process reads the first scenario's mainline. Hosting this next to
//! `a_head_merged_mid_run_is_observed` made
//! `assert_ne!(booted, merged)` fail whenever that sibling had already
//! moved `heads/main` to the same digest (#5000).

mod common;
pub mod fixture;
mod harness;

use std::thread;
use std::time::{Duration, Instant};

use fixture::{FixtureHarness, digest};

/// The commit a person merges straight to mainline. Recognizable, and no bloom
/// of this coordinator's ever names it — the point being that the coordinator
/// learns of a head it did not produce.
const MERGED: u8 = 0x7E;

#[test]
fn observe_tick_fires_on_the_coordinator_poll_cadence() {
    let mut harness = FixtureHarness::start_with_poll("observe-tick-fires-on-the-cadence", 1);

    let booted = harness.view().mainline;
    let merged = digest(MERGED);
    assert_ne!(booted, merged, "the merged head has to be one the boot observation did not already read");

    harness.move_mainline(merged);

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if harness.view().mainline == merged {
            return;
        }
        assert!(Instant::now() < deadline, "the poll timer never carried the moved head into the snapshot");
        thread::sleep(Duration::from_millis(50));
    }
}
