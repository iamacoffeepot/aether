//! Fixture-root lifetime (#5691).
//!
//! The last owner of a fixture's journal / worktree roots reaps recorded lane
//! groups before those directories delete. An earlier `HarnessRoots` drop while
//! a live harness still holds the same owner must leave the directories in
//! place. The fixture guard is armed before `start` so a panic during wait
//! cannot let `TempDir` drop under an unowned lane.

#![cfg(all(feature = "github", any(target_os = "linux", target_os = "macos")))]

use std::fs;
use std::io::{self, Write as _};
use std::mem;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneScript};
use aether_chassis_bloomery::bloomery::{GroupAbsence, ProcessIdentity, strict_group_absence};
use aether_harness_bloomery::{HarnessBuilder, HarnessRoots, ScenarioHarness};

/// Between polls for a matching identity record.
const IDENTITY_POLL: Duration = Duration::from_millis(50);
/// Ceiling for the identity file to appear and attach.
const IDENTITY_BUDGET: Duration = Duration::from_secs(20);

/// Owns the `NeverExits` fixture for the whole test, including setup panics.
///
/// Unwind: drop the harness (coordinator) first, then reap only a freshly
/// matching recorded identity. No matching live identity → forget the roots
/// so their `TempDir`s do not delete under an unowned child.
struct FixtureGuard {
    harness: Option<ScenarioHarness>,
    roots: Option<HarnessRoots>,
    identity: Option<ProcessIdentity>,
    runs: PathBuf,
}

impl FixtureGuard {
    fn begin() -> Self {
        let roots = HarnessRoots::create();
        let runs = roots.runs_path().to_owned();
        Self { harness: None, roots: Some(roots), identity: None, runs }
    }

    fn start_never_exits(&mut self) {
        let roots = self.roots.as_ref().expect("the guard holds roots before start");
        self.harness = Some(
            HarnessBuilder::lane(&LaneScript::all_passing().with_default(LaneMode::NeverExits))
                .wall_clock_secs(86_400)
                .roots(roots)
                .start("fixture-roots-never-exits"),
        );
    }

    fn wait_for_matching_identity(&mut self) {
        let deadline = Instant::now() + IDENTITY_BUDGET;
        loop {
            if let Some(recorded) = matching_identity_under(&self.runs) {
                self.identity = Some(recorded);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no matching native identity under the fixture runs root {}",
                self.runs.display(),
            );
            thread::sleep(IDENTITY_POLL);
        }
    }

    fn stop_harness(&mut self) {
        drop(self.harness.take());
    }

    /// Drop the last external `HarnessRoots`. Idempotent.
    fn release_roots(&mut self) {
        drop(self.roots.take());
    }

    fn preserve_roots(&mut self, why: &str) {
        if let Some(roots) = self.roots.take() {
            let _ = writeln!(io::stderr().lock(), "fixture-roots: preserving {} ({why})", self.runs.display());
            mem::forget(roots);
        }
    }
}

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        drop(self.harness.take());
        match self.identity.as_ref().and_then(ProcessIdentity::attach) {
            Some(live) => match live.terminate_group() {
                Ok(()) if strict_group_absence(live.pgid) == GroupAbsence::Absent => {
                    drop(self.roots.take());
                }
                Ok(()) => self.preserve_roots("terminate_group returned Ok but strict group absence is not Absent"),
                Err(_) => self.preserve_roots("terminate_group failed"),
            },
            None => self.preserve_roots("no matching live identity to reap"),
        }
    }
}

#[test]
fn dropping_a_never_exits_harness_must_leave_its_group_absent_and_remove_roots() {
    let mut guard = FixtureGuard::begin();
    guard.start_never_exits();
    guard.wait_for_matching_identity();
    let pgid = guard.identity.as_ref().expect("a matching identity is recorded").pgid;
    let runs = guard.runs.clone();

    guard.stop_harness();
    guard.release_roots();

    assert_eq!(
        strict_group_absence(pgid),
        GroupAbsence::Absent,
        "the last owner of the fixture roots must reap the recorded NeverExits group",
    );
    assert!(!runs.exists(), "reaped fixture roots must be removed");
}

#[test]
fn dropping_a_forked_coordinator_while_roots_are_held_leaves_the_lane_until_the_last_owner_reaps() {
    let mut guard = FixtureGuard::begin();
    guard.start_never_exits();
    guard.wait_for_matching_identity();
    let pgid = guard.identity.as_ref().expect("a matching identity is recorded").pgid;
    let runs = guard.runs.clone();

    guard.stop_harness();

    assert_eq!(
        strict_group_absence(pgid),
        GroupAbsence::Occupied,
        "a retained HarnessRoots must leave the recorded NeverExits group alive after the coordinator is killed",
    );
    assert!(runs.exists(), "retained roots must keep the worktree base");

    guard.release_roots();

    assert_eq!(
        strict_group_absence(pgid),
        GroupAbsence::Absent,
        "the last owner of the fixture roots must reap the recorded NeverExits group",
    );
    assert!(!runs.exists(), "reaped fixture roots must be removed");
}

#[test]
fn dropping_harness_roots_while_a_live_harness_still_uses_them_keeps_the_directories() {
    let roots = HarnessRoots::create();
    let store = PathBuf::from(roots.store_path());
    let runs = roots.runs_path().to_owned();
    let _harness = HarnessBuilder::fixture().roots(&roots).start("fixture-roots-dropped-before-harness");

    drop(roots);

    assert!(
        store.exists() && runs.exists(),
        "HarnessRoots path copies must not delete directories a live harness still holds",
    );
}

fn matching_identity_under(runs: &Path) -> Option<ProcessIdentity> {
    let entries = fs::read_dir(runs).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.file_name()?.to_str()?.ends_with("-evidence") {
            continue;
        }
        let recorded = ProcessIdentity::read(&path)?;
        recorded.attach()?;
        return Some(recorded);
    }
    None
}
