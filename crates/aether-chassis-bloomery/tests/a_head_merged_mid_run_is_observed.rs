//! A commit merged to mainline by a person reaches the coordinator's snapshot
//! while it keeps running.
//!
//! The observation used to be sent once, at boot, so the only thing that moved
//! the pointer after a merge was a restart — an operating step, in a loop that
//! is otherwise unattended. This drives the steady-state path: one live
//! coordinator, a head that moves under it, and the observer wake that carries
//! the change into the snapshot without the process going anywhere.
//!
//! The boot half rides along rather than being asserted twice.
//! `FixtureHarness::start` only returns once mainline has bound to a commit the
//! repository actually holds, which is the boot observation having run — so a
//! poll-driven observation that displaced it would fail before this scenario's
//! own subject is reached.

use aether_harness_bloomery::{FixtureHarness, digest};

/// The commit a person merges straight to mainline. Recognizable, and no bloom
/// of this coordinator's ever names it — the point being that the coordinator
/// learns of a head it did not produce.
const MERGED: u8 = 0x7E;

#[test]
fn a_head_merged_mid_run_reaches_the_snapshot_without_a_restart() {
    let mut harness = FixtureHarness::start("a-head-merged-mid-run-is-observed");

    let booted = harness.view().mainline;
    let merged = digest(MERGED);
    assert_ne!(booted, merged, "the merged head has to be one the boot observation did not already read");

    // Nothing between here and the assertion restarts the chassis, reopens a
    // store, or re-runs boot: the same process that read `booted` reads the
    // merge.
    harness.move_mainline(merged);
    harness.await_mainline(merged);
}
