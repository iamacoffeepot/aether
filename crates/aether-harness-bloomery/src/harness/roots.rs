//! Shared fixture journal / artifacts / lane-worktree roots.
//!
//! The last [`Arc`] owner reaps recorded lane groups under the runs root before
//! the `TempDir`s delete. An intermediate harness drop while another owner still
//! holds the [`Arc`] leaves those groups alive.

use std::fs;
use std::io::{self, ErrorKind, Write as _};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;

use aether_chassis_bloomery::bloomery::{GroupAbsence, IDENTITY_RECORD, ProcessIdentity, strict_group_absence};
use tempfile::TempDir;

/// Must match `EVIDENCE_SUFFIX` in the local executor backend.
const EVIDENCE_SUFFIX: &str = "-evidence";

/// Journal, artifacts, and lane worktrees for one fixture.
pub(super) struct FixtureRoots {
    state: Option<TempDir>,
    runs: Option<TempDir>,
}

impl FixtureRoots {
    pub(super) fn create_arc() -> Arc<Self> {
        Arc::new(Self {
            state: Some(tempfile::tempdir().expect("journal and artifacts root")),
            runs: Some(tempfile::tempdir().expect("lane worktree base")),
        })
    }

    pub(super) fn store_path(&self) -> String {
        self.state_dir().join("bloomery.db").to_string_lossy().into_owned()
    }

    pub(super) fn artifacts_root(&self) -> String {
        self.state_dir().join("artifacts").to_string_lossy().into_owned()
    }

    pub(super) fn worktree_base(&self) -> String {
        self.runs_dir().to_string_lossy().into_owned()
    }

    pub(super) fn runs_path(&self) -> &Path {
        self.runs_dir()
    }

    fn state_dir(&self) -> &Path {
        self.state.as_ref().expect("fixture state is live").path()
    }

    fn runs_dir(&self) -> &Path {
        self.runs.as_ref().expect("fixture runs are live").path()
    }
}

impl Drop for FixtureRoots {
    fn drop(&mut self) {
        let Some(runs) = self.runs.as_ref() else {
            return;
        };
        let display = runs.path().display().to_string();
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| reap_owned_lanes(runs.path())));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(why)) => {
                let _ = writeln!(io::stderr().lock(), "fixture-roots: preserving {display} ({why})");
                keep_both(self);
            }
            Err(_) => {
                let _ = writeln!(
                    io::stderr().lock(),
                    "fixture-roots: preserving {display} (panic during owned-lane reap)",
                );
                keep_both(self);
            }
        }
    }
}

fn keep_both(roots: &mut FixtureRoots) {
    if let Some(state) = roots.state.take() {
        let _ = state.keep();
    }
    if let Some(runs) = roots.runs.take() {
        let _ = runs.keep();
    }
}

fn reap_owned_lanes(runs: &Path) -> Result<(), String> {
    let listing = fs::read_dir(runs).map_err(|error| format!("runs directory unreadable: {error}"))?;
    let mut footprints = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|error| format!("runs entry unreadable: {error}"))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|error| format!("{}: metadata: {error}", path.display()))?;
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Err(format!("{}: name is not UTF-8", path.display()));
        };
        if !name.ends_with(EVIDENCE_SUFFIX) {
            continue;
        }
        if meta.file_type().is_symlink() {
            return Err(format!("{name}: evidence footprint is a symlink"));
        }
        if !meta.is_dir() {
            return Err(format!("{name}: evidence footprint is not a directory"));
        }
        footprints.push(path);
    }

    for footprint in &footprints {
        reap_footprint(footprint)?;
    }
    Ok(())
}

fn reap_footprint(evidence_dir: &Path) -> Result<(), String> {
    let Some(recorded) = read_recorded_identity(evidence_dir)? else {
        return Err(format!("{}: identity is missing or unreadable", evidence_dir.display()));
    };
    match recorded.attach() {
        Some(live) if live.pgid != recorded.pgid => {
            Err(format!("{}: attached process changed its process group", evidence_dir.display()))
        }
        Some(live) => {
            live.terminate_group().map_err(|error| format!("{}: terminate_group: {error}", evidence_dir.display()))?;
            require_strict_absent(recorded.pgid, evidence_dir)
        }
        None => require_strict_absent(recorded.pgid, evidence_dir),
    }
}

fn require_strict_absent(pgid: u32, evidence_dir: &Path) -> Result<(), String> {
    match strict_group_absence(pgid) {
        GroupAbsence::Absent => Ok(()),
        other => Err(format!("{}: strict group absence is {other:?}", evidence_dir.display())),
    }
}

fn read_recorded_identity(evidence_dir: &Path) -> Result<Option<ProcessIdentity>, String> {
    let path = evidence_dir.join(IDENTITY_RECORD);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(format!("{}: identity is a symlink", path.display()));
    }
    if !meta.is_file() {
        return Err(format!("{}: identity is not a file", path.display()));
    }
    ProcessIdentity::read(evidence_dir).map(Some).ok_or_else(|| format!("{}: identity is unreadable", path.display()))
}

/// Where the journal, artifacts, and lane worktrees live. Own one when a
/// scenario must drop a coordinator and boot another against the same files.
///
/// Dropping the last owner reaps recorded lane groups under the runs root,
/// then deletes the directories. An earlier drop while a harness still holds
/// the same owner leaves those groups alive.
pub struct HarnessRoots {
    inner: Arc<FixtureRoots>,
}

impl HarnessRoots {
    /// Fresh temporary roots. The last `Arc` owner reclaims them.
    ///
    /// # Panics
    /// A temporary directory could not be created.
    #[must_use]
    pub fn create() -> Self {
        Self { inner: FixtureRoots::create_arc() }
    }

    pub(super) fn owner(&self) -> Arc<FixtureRoots> {
        Arc::clone(&self.inner)
    }

    /// The journal file the store and every reactor open.
    #[must_use]
    pub fn store_path(&self) -> String {
        self.inner.store_path()
    }

    /// The artifacts content-store root.
    #[must_use]
    pub fn artifacts_root(&self) -> String {
        self.inner.artifacts_root()
    }

    /// The scratch-worktree base the local lane checks each order into.
    #[must_use]
    pub fn worktree_base(&self) -> String {
        self.inner.worktree_base()
    }

    /// The worktree base as a path, for writing a [`LaneScript`].
    ///
    /// [`LaneScript`]: aether_chassis_bloomery::bloomery::mock_lane::LaneScript
    #[must_use]
    pub fn runs_path(&self) -> &Path {
        self.inner.runs_path()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use aether_chassis_bloomery::bloomery::mock_lane::LaneScript;
    use aether_chassis_bloomery::bloomery::{GroupAbsence, IDENTITY_RECORD, ProcessIdentity, strict_group_absence};

    use super::{EVIDENCE_SUFFIX, HarnessRoots};
    use crate::harness::HarnessBuilder;

    fn state_and_runs(roots: &HarnessRoots) -> (PathBuf, PathBuf) {
        (
            PathBuf::from(roots.store_path()).parent().expect("store path has a parent").to_owned(),
            roots.runs_path().to_owned(),
        )
    }

    fn evidence_dir(runs: &Path, nonce: &str) -> PathBuf {
        runs.join(format!("{nonce}{EVIDENCE_SUFFIX}"))
    }

    fn remove_kept(state: &Path, runs: &Path) {
        fs::remove_dir_all(runs).expect("retained runs root is removed after the test");
        fs::remove_dir_all(state).expect("retained state root is removed after the test");
    }

    fn assert_both_retained(state: &Path, runs: &Path, why: &str) {
        assert!(state.exists() && runs.exists(), "{why}");
    }

    fn write_unsafe_group_identity(evidence: &Path, pgid: u32) {
        ProcessIdentity { pid: pgid, pgid, starttime: 1, boot_id: "fixture-roots-test".to_owned() }
            .write(evidence)
            .expect("the unsafe-group identity writes");
    }

    #[test]
    fn dropping_harness_roots_while_the_builder_still_holds_the_owner_keeps_the_directories() {
        let roots = HarnessRoots::create();
        let (state, runs) = state_and_runs(&roots);
        let builder = HarnessBuilder::lane(&LaneScript::all_passing()).roots(&roots);
        drop(roots);
        assert_both_retained(&state, &runs, "the builder's retained owner must keep both directories");
        drop(builder);
        assert!(!state.exists() && !runs.exists(), "the last owner of empty roots deletes both directories");
    }

    #[test]
    fn a_missing_identity_in_an_evidence_footprint_retains_the_roots() {
        let roots = HarnessRoots::create();
        let (state, runs) = state_and_runs(&roots);
        fs::create_dir_all(evidence_dir(&runs, "n-missing")).expect("an evidence footprint");
        drop(roots);
        assert_both_retained(&state, &runs, "a missing identity must retain both roots");
        remove_kept(&state, &runs);
    }

    #[test]
    fn an_invalid_identity_in_an_evidence_footprint_retains_the_roots() {
        let roots = HarnessRoots::create();
        let (state, runs) = state_and_runs(&roots);
        let evidence = evidence_dir(&runs, "n-invalid");
        fs::create_dir_all(&evidence).expect("an evidence footprint");
        fs::write(evidence.join(IDENTITY_RECORD), "{not json").expect("invalid identity bytes");
        drop(roots);
        assert_both_retained(&state, &runs, "an invalid identity must retain both roots");
        remove_kept(&state, &runs);
    }

    #[test]
    fn an_unsafe_recorded_group_retains_both_roots_without_signaling() {
        for pgid in [0, 1] {
            let roots = HarnessRoots::create();
            let (state, runs) = state_and_runs(&roots);
            let evidence = evidence_dir(&runs, "n-broadcast");
            fs::create_dir_all(&evidence).expect("an evidence footprint");
            write_unsafe_group_identity(&evidence, pgid);
            drop(roots);
            assert_both_retained(&state, &runs, "recorded group 0/1 must retain both roots");
            assert_eq!(strict_group_absence(pgid), GroupAbsence::Unknown);
            remove_kept(&state, &runs);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_evidence_footprint_retains_both_roots() {
        let roots = HarnessRoots::create();
        let (state, runs) = state_and_runs(&roots);
        let real = runs.join("ordinary-directory");
        fs::create_dir_all(&real).expect("a non-evidence directory");
        symlink(&real, evidence_dir(&runs, "n-link")).expect("an evidence symlink");
        drop(roots);
        assert_both_retained(&state, &runs, "a symlink evidence footprint must retain both roots");
        remove_kept(&state, &runs);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_identity_record_retains_both_roots() {
        let roots = HarnessRoots::create();
        let (state, runs) = state_and_runs(&roots);
        let evidence = evidence_dir(&runs, "n-id-link");
        fs::create_dir_all(&evidence).expect("an evidence footprint");
        let target = evidence.join("target");
        fs::write(&target, b"{}").expect("a symlink target");
        symlink(&target, evidence.join(IDENTITY_RECORD)).expect("an identity symlink");
        drop(roots);
        assert_both_retained(&state, &runs, "a symlink identity record must retain both roots");
        remove_kept(&state, &runs);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    mod live {
        use std::os::unix::process::CommandExt as _;
        use std::panic::{self, AssertUnwindSafe};
        use std::process::{Child, Command, Stdio};

        use super::*;

        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn spawn_group_sleep() -> (ChildGuard, ProcessIdentity) {
            let child = Command::new("sleep")
                .arg("60")
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the probe child forks");
            let guard = ChildGuard(child);
            let identity = ProcessIdentity::observe(guard.0.id()).expect("the child is live long enough to observe");
            assert_eq!(identity.pgid, guard.0.id(), "process_group(0) makes the child its own group leader");
            assert!(identity.pgid > 1, "the child must own a private group");
            (guard, identity)
        }

        #[test]
        fn a_mismatched_identity_retains_roots_and_leaves_the_occupying_child_alive() {
            let roots = HarnessRoots::create();
            let (state, runs) = state_and_runs(&roots);
            let (mut child, mut identity) = spawn_group_sleep();
            identity.starttime = identity.starttime.wrapping_add(1);
            let evidence = evidence_dir(&runs, "n-mismatch");
            fs::create_dir_all(&evidence).expect("an evidence footprint");
            identity.write(&evidence).expect("the mismatched identity writes");

            drop(roots);

            assert_both_retained(&state, &runs, "a mismatched identity must retain both roots");
            assert!(child.0.try_wait().expect("try_wait").is_none(), "the occupying child must still be live");
            child.0.kill().expect("the occupying child is signalled");
            child.0.wait().expect("the occupying child is reaped");
            remove_kept(&state, &runs);
        }

        #[test]
        fn a_matching_identity_whose_child_has_been_waited_out_permits_deletion() {
            let roots = HarnessRoots::create();
            let (state, runs) = state_and_runs(&roots);
            let (mut child, identity) = spawn_group_sleep();
            let evidence = evidence_dir(&runs, "n-waited");
            fs::create_dir_all(&evidence).expect("an evidence footprint");
            identity.write(&evidence).expect("the matching identity writes");
            child.0.kill().expect("the child is signalled");
            child.0.wait().expect("the child is reaped");

            drop(roots);

            assert!(
                !state.exists() && !runs.exists(),
                "a waited-out matching group that is strictly absent may delete both roots"
            );
            assert_eq!(strict_group_absence(identity.pgid), GroupAbsence::Absent);
        }

        #[test]
        fn the_last_owner_of_one_fixture_does_not_reap_another() {
            let first = HarnessRoots::create();
            let second = HarnessRoots::create();
            let (first_state, first_runs) = state_and_runs(&first);
            let (second_state, second_runs) = state_and_runs(&second);
            let (mut child_a, identity_a) = spawn_group_sleep();
            let (mut child_b, identity_b) = spawn_group_sleep();
            let evidence_a = evidence_dir(&first_runs, "n-a");
            let evidence_b = evidence_dir(&second_runs, "n-b");
            fs::create_dir_all(&evidence_a).expect("first evidence");
            fs::create_dir_all(&evidence_b).expect("second evidence");
            identity_a.write(&evidence_a).expect("first identity");
            identity_b.write(&evidence_b).expect("second identity");

            drop(first);

            assert_eq!(
                strict_group_absence(identity_a.pgid),
                GroupAbsence::Absent,
                "the first last-owner reaps its group"
            );
            assert_eq!(
                strict_group_absence(identity_b.pgid),
                GroupAbsence::Occupied,
                "the second fixture's group must still be live",
            );
            assert_both_retained(&second_state, &second_runs, "the second fixture's roots must still exist");
            assert!(!first_state.exists() && !first_runs.exists(), "the first last-owner deletes both of its roots");
            assert!(
                child_a.0.try_wait().expect("try_wait").is_some(),
                "last-owner terminated the first group; try_wait reaped the child",
            );

            drop(second);

            assert_eq!(strict_group_absence(identity_b.pgid), GroupAbsence::Absent);
            assert!(
                child_b.0.try_wait().expect("try_wait").is_some(),
                "last-owner terminated the second group; try_wait reaped the child",
            );
            assert!(!second_state.exists() && !second_runs.exists());
        }

        #[test]
        fn an_extra_arc_clone_is_not_the_last_owner() {
            let roots = HarnessRoots::create();
            let (state, runs) = state_and_runs(&roots);
            let (mut child, identity) = spawn_group_sleep();
            let evidence = evidence_dir(&runs, "n-held");
            fs::create_dir_all(&evidence).expect("an evidence footprint");
            identity.write(&evidence).expect("the matching identity writes");
            let retained = Arc::clone(&roots.inner);

            drop(roots);

            assert_eq!(strict_group_absence(identity.pgid), GroupAbsence::Occupied);
            assert_both_retained(&state, &runs, "a retained owner must keep both directories");
            drop(retained);
            assert_eq!(strict_group_absence(identity.pgid), GroupAbsence::Absent);
            assert!(
                child.0.try_wait().expect("try_wait").is_some(),
                "last-owner terminated the group; try_wait reaped the child",
            );
            assert!(!state.exists() && !runs.exists());
        }

        #[test]
        fn dropping_last_roots_during_unwind_reaps_an_owned_group_and_removes_both_roots() {
            let roots = HarnessRoots::create();
            let (state, runs) = state_and_runs(&roots);
            let (mut child, identity) = spawn_group_sleep();
            let evidence = evidence_dir(&runs, "n-unwind");
            fs::create_dir_all(&evidence).expect("an evidence footprint");
            identity.write(&evidence).expect("the matching identity writes");

            let unwound = panic::catch_unwind(AssertUnwindSafe(move || {
                let _last = roots;
                panic!("a failing assert while the test still owns the last roots");
            }));

            assert!(unwound.is_err(), "the scope holding the last roots panicked");
            assert!(
                child.0.try_wait().expect("try_wait").is_some(),
                "unwind drop terminated the owned group; try_wait reaped the child",
            );
            assert!(!state.exists() && !runs.exists(), "unwind drop removed both roots");
        }
    }
}
