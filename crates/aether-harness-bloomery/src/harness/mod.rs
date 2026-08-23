//! One scenario harness, with variation as builder axes.
//!
//! The three hand-rolled harnesses were the same loop — `BloomeryEnv`,
//! boot-and-wait, `view`/`call`, seal, tempdirs — with three cells of
//! substitution. A fourth cell used to mean a fourth copy. Reach here instead:
//! pick the axes, or pick a named cell that already has them set.
//!
//! # Axes
//!
//! | Axis | Values | What it selects |
//! | --- | --- | --- |
//! | **backend** | `fixture` / `local repo` | In-memory GitHub (`shared_fixture`) versus a real git repository the source port can check out |
//! | **coordinator** | `in-process` / `forked` | `BloomeryChassis::build` in the test process versus a `bloomery` child |
//! | **lane** | `off` / `scripted` | No local lane (scripted verdicts over the wire) versus `bloomery-mock-lane` as `AETHER_BLOOMERY_LANE_PROGRAM` |
//!
//! # Named cells
//!
//! | Cell | Axes | Reach for it when |
//! | --- | --- | --- |
//! | [`FixtureHarness`] | fixture + in-process + off | A reactor-to-reactor handoff, driven one explicit tick at a time. One per test binary: `shared_fixture` is process-global. |
//! | [`LaneHarness`] | local repo + forked + scripted | The durable work loop below the spawn: `git worktree add`, the child, `evidence.json`, candidate capture. |
//! | [`HarnessBuilder::local_authority`] | local repo + in-process + scripted | A fleet-local bare authority, real capture and publication, a restart against the same journal. The proof that a new cell is a builder line, not a new harness. |
//!
//! A new scenario picks a cell. It does not write a fourth `struct Harness`.
//!
//! Shared pieces live next door: [`Repo`] for a real
//! repository, [`Wire`](crate::support::wire::Wire) for handshake-retry / `call` /
//! view, [`MapCorrespondence`](crate::support::correspondence::MapCorrespondence)
//! for an in-memory correspondence store.
//!
//! Consumers declare `pub mod harness` so the cell surface stays reachable in
//! every binary that compiles it — the same load-bearing visibility as
//! `pub mod fixture`.

pub mod drive;
mod operator;
mod scenario;

use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use aether_chassis_bloomery::bloomery::mock_lane::LaneScript;
use tempfile::TempDir;

use crate::support::repo::Repo;

#[doc(inline)]
pub use aether_bloomery::testing::digest;
pub use drive::{draft, passed};
pub use scenario::ScenarioHarness;

/// The promoted [`ScenarioHarness`], named to match the crate the way
/// `FleetHarness` matches its own.
pub type BloomeryHarness = ScenarioHarness;

/// A poll cadence far enough out that no reactor's timer fires inside a
/// scenario. There is no "never" — `poll_interval_secs.max(1)` — so a day
/// stands in for one.
const QUIET_POLL_SECS: u64 = 86_400;

/// How long the source cap's boot reconcile may take to bind mainline.
const BOOT_BUDGET: Duration = Duration::from_secs(20);

/// Between re-wakes inside an in-process waiting step.
const POLL: Duration = Duration::from_millis(20);

/// In-process socket read timeout, set well clear of the step budget so a slow
/// tick reports the budget rather than an io timeout.
const SOCKET_READ_TIMEOUT: Duration = Duration::from_mins(2);

/// `GithubConnectionConfig::shared_fixture` is a process-global `OnceLock`.
/// A second fixture-backend start in this process would share the first
/// scenario's repository and mainline — the #5000 flake.
static HARNESS_STARTED: AtomicBool = AtomicBool::new(false);

/// Where the journal, artifacts, and lane worktrees live. Own one when a
/// scenario must drop a coordinator and boot another against the same files.
pub struct HarnessRoots {
    state: TempDir,
    runs: TempDir,
}

impl HarnessRoots {
    /// Fresh temporary roots. Dropping this after the last harness is what
    /// reclaims them on the unwind path.
    ///
    /// # Panics
    /// A temporary directory could not be created.
    #[must_use]
    pub fn create() -> Self {
        Self {
            state: tempfile::tempdir().expect("journal and artifacts root"),
            runs: tempfile::tempdir().expect("lane worktree base"),
        }
    }

    /// The journal file the store and every reactor open.
    #[must_use]
    pub fn store_path(&self) -> String {
        self.state.path().join("bloomery.db").to_string_lossy().into_owned()
    }

    /// The artifacts content-store root.
    #[must_use]
    pub fn artifacts_root(&self) -> String {
        self.state.path().join("artifacts").to_string_lossy().into_owned()
    }

    /// The scratch-worktree base the local lane checks each order into.
    #[must_use]
    pub fn worktree_base(&self) -> String {
        self.runs.path().to_string_lossy().into_owned()
    }

    /// The worktree base as a path, for writing a [`LaneScript`].
    #[must_use]
    pub fn runs_path(&self) -> &Path {
        self.runs.path()
    }
}

/// Backend axis: in-memory GitHub, or a real git repository.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    /// `github_backend = fixture`. Process-global `shared_fixture` — one start
    /// per test binary.
    Fixture,
    /// A real repository the source port can check out.
    LocalRepo,
}

/// Coordinator axis: boot in this process, or fork the production bin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoordinatorKind {
    /// `BloomeryChassis::build` — owns the roots, explicit ticks.
    InProcess,
    /// Forked `bloomery` child — production boot, own cwd, own poll cadence.
    Forked,
}

/// Lane axis: no local lane, or the mock-lane binary at the end of the argv.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lane {
    /// `local_lane_enabled = false`. Verdicts arrive as scripted uploads.
    Off,
    /// `bloomery-mock-lane` is `AETHER_BLOOMERY_LANE_PROGRAM`.
    Scripted,
}

/// Builder for [`ScenarioHarness`]. Start from a named cell, then override an
/// axis or a knob; do not start from a blank and rediscover the cell.
pub struct HarnessBuilder {
    backend: Backend,
    coordinator: CoordinatorKind,
    lane: Lane,
    poll_interval_secs: u64,
    cas_land_enabled: bool,
    auto_seal: bool,
    workpiece: String,
    wall_clock_secs: Option<u64>,
    heartbeat_silence_secs: Option<u64>,
    script: Option<LaneScript>,
    repo: Option<Repo>,
    authority_path: Option<PathBuf>,
    shared_store: Option<String>,
    shared_artifacts: Option<String>,
    shared_worktree: Option<String>,
    operator_name: String,
    operator_email: String,
    github_fixture: bool,
    socket_read_timeout: Option<Duration>,
    step_budget: Duration,
}

impl HarnessBuilder {
    /// Fixture cell: in-memory GitHub, in-process chassis, lane off.
    #[must_use]
    pub fn fixture() -> Self {
        Self {
            backend: Backend::Fixture,
            coordinator: CoordinatorKind::InProcess,
            lane: Lane::Off,
            poll_interval_secs: QUIET_POLL_SECS,
            cas_land_enabled: true,
            auto_seal: false,
            workpiece: "wp".to_owned(),
            wall_clock_secs: None,
            heartbeat_silence_secs: None,
            script: None,
            repo: None,
            authority_path: None,
            shared_store: None,
            shared_artifacts: None,
            shared_worktree: None,
            operator_name: String::new(),
            operator_email: String::new(),
            github_fixture: true,
            socket_read_timeout: Some(SOCKET_READ_TIMEOUT),
            step_budget: Duration::from_secs(20),
        }
    }

    /// Lane-boundary cell: scratch repo, forked coordinator, scripted mock lane.
    #[must_use]
    pub fn lane(script: &LaneScript) -> Self {
        Self {
            backend: Backend::LocalRepo,
            coordinator: CoordinatorKind::Forked,
            lane: Lane::Scripted,
            poll_interval_secs: 1,
            cas_land_enabled: true,
            auto_seal: true,
            workpiece: "wp".to_owned(),
            wall_clock_secs: None,
            heartbeat_silence_secs: None,
            script: Some(script.clone()),
            repo: None,
            authority_path: None,
            shared_store: None,
            shared_artifacts: None,
            shared_worktree: None,
            operator_name: "lane harness".to_owned(),
            operator_email: "lane-harness@example.test".to_owned(),
            github_fixture: true,
            socket_read_timeout: None,
            step_budget: Duration::from_mins(2),
        }
    }

    /// Local-authority cell: bare repo, in-process chassis, scripted mock lane.
    ///
    /// Pass the same [`HarnessRoots`] on a second start to model a restart.
    #[must_use]
    pub fn local_authority(repo: &Repo) -> Self {
        Self {
            backend: Backend::LocalRepo,
            coordinator: CoordinatorKind::InProcess,
            lane: Lane::Scripted,
            poll_interval_secs: QUIET_POLL_SECS,
            cas_land_enabled: true,
            auto_seal: false,
            workpiece: "wp".to_owned(),
            wall_clock_secs: None,
            heartbeat_silence_secs: None,
            script: Some(LaneScript::all_passing()),
            repo: None,
            authority_path: Some(repo.path().to_owned()),
            shared_store: None,
            shared_artifacts: None,
            shared_worktree: None,
            operator_name: "local-authority harness".to_owned(),
            operator_email: "local-authority@example.test".to_owned(),
            github_fixture: false,
            socket_read_timeout: Some(SOCKET_READ_TIMEOUT),
            step_budget: Duration::from_secs(30),
        }
    }

    /// Override the backend axis.
    #[must_use]
    pub const fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    /// Override the coordinator axis.
    #[must_use]
    pub const fn coordinator(mut self, coordinator: CoordinatorKind) -> Self {
        self.coordinator = coordinator;
        self
    }

    /// Override the lane axis.
    #[must_use]
    pub const fn lane_axis(mut self, lane: Lane) -> Self {
        self.lane = lane;
        self
    }

    /// Observer / reactor poll cadence, in seconds.
    #[must_use]
    pub const fn poll_interval_secs(mut self, secs: u64) -> Self {
        self.poll_interval_secs = secs;
        self
    }

    /// Compare-and-swap land gate. Off so a scenario can observe `Resolved`
    /// before the land reactor consumes it.
    #[must_use]
    pub const fn cas_land(mut self, enabled: bool) -> Self {
        self.cas_land_enabled = enabled;
        self
    }

    /// Workpiece the auto-sealed member covers (lane cell).
    #[must_use]
    pub fn workpiece(mut self, workpiece: impl Into<String>) -> Self {
        self.workpiece = workpiece.into();
        self
    }

    /// Seal a stage catalog binding every stage's wall-clock at `secs`.
    #[must_use]
    pub const fn wall_clock_secs(mut self, secs: u64) -> Self {
        self.wall_clock_secs = Some(secs);
        self
    }

    /// Host heartbeat-silence allowance, in seconds.
    #[must_use]
    pub const fn heartbeat_silence_secs(mut self, secs: u64) -> Self {
        self.heartbeat_silence_secs = Some(secs);
        self
    }

    /// Script the mock lane reads. Ignored when the lane axis is off.
    #[must_use]
    pub fn script(mut self, script: &LaneScript) -> Self {
        self.script = Some(script.clone());
        self
    }

    /// Reuse journal / artifacts / worktree roots across a restart.
    #[must_use]
    pub fn roots(mut self, roots: &HarnessRoots) -> Self {
        self.shared_store = Some(roots.store_path());
        self.shared_artifacts = Some(roots.artifacts_root());
        self.shared_worktree = Some(roots.worktree_base());
        self
    }

    /// Keep `repo` alive for the life of the harness — local-authority start
    /// copies the path but the `TempDir` still has to outlive the coordinator.
    #[must_use]
    pub fn hold_repo(mut self, repo: Repo) -> Self {
        self.repo = Some(repo);
        self
    }

    /// Boot the cell.
    ///
    /// # Panics
    /// Setup failed, the chassis did not boot, or mainline never bound.
    #[must_use]
    pub fn start(self, client_name: &str) -> ScenarioHarness {
        ScenarioHarness::boot(self, client_name)
    }
}

/// The fixture cell of [`ScenarioHarness`].
pub struct FixtureHarness {
    inner: ScenarioHarness,
}

impl FixtureHarness {
    /// Boot a coordinator over fresh temporary stores and the process-global
    /// in-memory repository, and hold it until its mainline is sealable.
    ///
    /// # Panics
    /// The chassis did not boot, the RPC ingress did not answer, or mainline
    /// never bound to a commit inside the boot budget. A second start in this
    /// process trips the one-fixture-per-binary assertion (#5000).
    #[must_use]
    pub fn start(client_name: &str) -> Self {
        Self { inner: HarnessBuilder::fixture().start(client_name) }
    }

    /// Boot like [`start`](Self::start), but on an explicit observer cadence.
    ///
    /// # Panics
    /// As [`start`](Self::start).
    #[must_use]
    pub fn start_with_poll(client_name: &str, poll_interval_secs: u64) -> Self {
        Self { inner: HarnessBuilder::fixture().poll_interval_secs(poll_interval_secs).start(client_name) }
    }
}

impl Deref for FixtureHarness {
    type Target = ScenarioHarness;

    fn deref(&self) -> &ScenarioHarness {
        &self.inner
    }
}

impl DerefMut for FixtureHarness {
    fn deref_mut(&mut self) -> &mut ScenarioHarness {
        &mut self.inner
    }
}

/// The lane-boundary cell of [`ScenarioHarness`].
pub struct LaneHarness {
    inner: ScenarioHarness,
}

impl LaneHarness {
    /// Boot a coordinator over a fresh scratch repository, seal a single-member
    /// bloom against it, and hand back the live harness.
    ///
    /// # Panics
    /// Any setup step failed, or the seal was refused.
    #[must_use]
    pub fn start(script: &LaneScript) -> Self {
        Self::start_with(script, "wp")
    }

    /// [`start`](Self::start), naming the workpiece the sealed member covers.
    ///
    /// # Panics
    /// As [`start`](Self::start).
    #[must_use]
    pub fn start_with(script: &LaneScript, workpiece: &str) -> Self {
        Self { inner: HarnessBuilder::lane(script).workpiece(workpiece).start("lane-boundary-harness") }
    }

    /// [`start`](Self::start) over a bloom that seals a stage catalog binding
    /// every stage's execution limit at `wall_clock_secs` (ADR-0177).
    ///
    /// # Panics
    /// As [`start`](Self::start).
    #[must_use]
    pub fn start_with_wall_clock(script: &LaneScript, wall_clock_secs: u64) -> Self {
        Self { inner: HarnessBuilder::lane(script).wall_clock_secs(wall_clock_secs).start("lane-boundary-harness") }
    }

    /// [`start_with_wall_clock`](Self::start_with_wall_clock) plus a host
    /// heartbeat-silence allowance.
    ///
    /// # Panics
    /// As [`start`](Self::start).
    #[must_use]
    pub fn start_with_heartbeat(script: &LaneScript, wall_clock_secs: u64, heartbeat_silence_secs: u64) -> Self {
        Self {
            inner: HarnessBuilder::lane(script)
                .wall_clock_secs(wall_clock_secs)
                .heartbeat_silence_secs(heartbeat_silence_secs)
                .start("lane-boundary-harness"),
        }
    }

    /// Poll until `want` holds of the (single) bloom, checking both liveness
    /// invariants on every poll.
    ///
    /// # Panics
    /// The budget expired, or the coordinator went quiescent with work still owed.
    pub fn settle(
        &mut self,
        label: &str,
        want: impl Fn(&aether_bloomery::BloomView) -> bool,
    ) -> aether_bloomery::BloomView {
        self.inner.settle(label, want)
    }
}

impl Deref for LaneHarness {
    type Target = ScenarioHarness;

    fn deref(&self) -> &ScenarioHarness {
        &self.inner
    }
}

impl DerefMut for LaneHarness {
    fn deref_mut(&mut self) -> &mut ScenarioHarness {
        &mut self.inner
    }
}

/// Drive `body` while a scoped worker keeps calling `tick`. The worker
/// stops when `body` returns or panics, so this is `thread::scope` rather
/// than a detached spawn the settlement umbrella would refuse.
pub fn while_pumping<T>(mut tick: impl FnMut() + Send, body: impl FnOnce() -> T) -> T {
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                tick();
                thread::sleep(Duration::from_millis(200));
            }
        });
        let _stop = StopOnDrop(&stop);
        body()
    })
}
