//! Boot construction for [`ScenarioHarness`]: env, coordinator, handshake, wait.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_bloomery::{
    BackendObjectId, BloomDraft, BloomId, BloomStatus, BloomView, CandidateRef, ConfigKind, ConfigRegistry,
    Correspondence, Digest, Evidence, EvidenceKind, Fact, Membership, Outcome, Snapshot, StageCatalog, ViewDocument,
    WorkpieceId,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitDataApi, PullRequestApi, candidate_ref_name, landing_branch, short_hex, to_hex};
use aether_chassis_bloomery::artifacts::{ArtifactsCapabilityState, ArtifactsConfig, GetResult};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneRun, read_ledger};
use aether_chassis_bloomery::bloomery::{
    BloomeryChassis, BloomeryEnv, Chassis, CoordinatorConfig, DispatchTick, ExecutorReactorCapability,
    GithubConnectionConfig, IntegrateReactorCapability, IntegrateTick, LandReactorCapability, LandTick,
    ScriptedEvidence, ScriptedEvidenceResult, ScriptedUpload,
};
use aether_chassis_bloomery::control::ObserveTick;
use aether_chassis_bloomery::session::SessionConfig;
use aether_chassis_bloomery::signing::SigningConfig;
use aether_chassis_bloomery::store::{OutstandingOrder, SqliteCorrespondence, SqliteStore, StoreBackend, StoreConfig};
use aether_data::Kind;
use aether_data::wire::to_vec;
use aether_rpc::RpcServerHandle;
use aether_substrate::chassis::builder::BuiltChassis;
use tempfile::TempDir;

use super::drive::member;
use super::{BOOT_BUDGET, Backend, CoordinatorKind, HARNESS_STARTED, HarnessBuilder, Lane, POLL, liveness};
use crate::common::client::connect_and_handshake;
use crate::common::repo::Repo;
use crate::common::wire::{Wire, control_mailbox};
use crate::common::{Coordinator, free_port};

/// How long the world must hold still, with nothing in flight, before a settle
/// loop calls it quiescent. Comfortably more than the poll cadence plus the gap
/// between consuming one order and dispatching the next.
const QUIESCENCE: Duration = Duration::from_secs(12);

/// Between polls of a forked coordinator's projection.
const SETTLE_POLL: Duration = Duration::from_millis(250);

/// A live scenario: a booted coordinator, the backend it runs against, and the
/// wire connection that drives and observes it.
pub struct ScenarioHarness {
    _chassis: Option<BuiltChassis<BloomeryChassis>>,
    _coordinator: Option<Coordinator>,
    _state: Option<TempDir>,
    _runs: Option<TempDir>,
    wire: Wire,
    fake: Option<FakeGithub>,
    repo: Option<Repo>,
    store_path: String,
    artifacts_root: PathBuf,
    worktree_base: String,
    base: Digest,
    step_budget: Duration,
}

impl ScenarioHarness {
    pub(super) fn boot(mut builder: HarnessBuilder, client_name: &str) -> Self {
        if builder.backend == Backend::Fixture && builder.coordinator == CoordinatorKind::InProcess {
            // Tripwire: two fixture-backend starts in one binary share
            // `shared_fixture`'s mainline. Sequential `--test-threads=1` made
            // `assert_ne!(booted, merged)` fail every time the first test had
            // already moved `heads/main` (#5000).
            assert!(
                !HARNESS_STARTED.swap(true, Ordering::SeqCst),
                "one FixtureHarness per test binary — shared_fixture is process-global"
            );
        }

        let owned_state;
        let owned_runs;
        let store_path;
        let artifacts_root;
        let worktree_base;
        if let (Some(store), Some(artifacts), Some(worktree)) =
            (&builder.shared_store, &builder.shared_artifacts, &builder.shared_worktree)
        {
            owned_state = None;
            owned_runs = None;
            store_path = store.clone();
            artifacts_root = artifacts.clone();
            worktree_base = worktree.clone();
        } else {
            let state = tempfile::tempdir().expect("a temporary root for the journal and the artifacts store");
            store_path = state.path().join("bloomery.db").to_string_lossy().into_owned();
            artifacts_root = state.path().join("artifacts").to_string_lossy().into_owned();
            let runs = tempfile::tempdir().expect("lane worktree base");
            worktree_base = runs.path().to_string_lossy().into_owned();
            owned_state = Some(state);
            owned_runs = Some(runs);
        }

        if let Some(script) = &builder.script {
            script.write_to(Path::new(&worktree_base)).expect("the mock-lane script writes");
        }

        let repo = match builder.backend {
            Backend::Fixture => None,
            Backend::LocalRepo if builder.authority_path.is_some() => None,
            Backend::LocalRepo => Some(builder.repo.take().unwrap_or_else(Repo::scratch)),
        };

        if builder.coordinator == CoordinatorKind::Forked
            && let Some(repo) = &repo
        {
            let base = Snapshot::GENESIS_MAINLINE;
            SqliteCorrespondence::open(&store_path)
                .expect("the correspondence store opens")
                .record(&base, &backend_object(repo.head()))
                .expect("genesis correspondence records");
        }

        let configs =
            builder.wall_clock_secs.map_or_else(ConfigRegistry::default, |secs| author_catalog(&store_path, secs));

        let (chassis, coordinator, wire, fake) = match builder.coordinator {
            CoordinatorKind::InProcess => {
                let env = in_process_env(&builder, &store_path, &artifacts_root, &worktree_base);
                let fake = builder.github_fixture.then(|| env.github.shared_fixture());
                let chassis = BloomeryChassis::build(env).expect("the coordinator boots");
                let port = chassis.handle::<RpcServerHandle>().expect("the RPC ingress published its port").local_port;
                let wire = Wire::connect(port, client_name);
                if let Some(timeout) = builder.socket_read_timeout {
                    wire.set_read_timeout(timeout);
                }
                (Some(chassis), None, wire, fake)
            }
            CoordinatorKind::Forked => {
                let repo = repo.as_ref().expect("the forked cell needs a scratch repository");
                let (child, stream) = spawn_listening_coordinator(
                    repo,
                    &worktree_base,
                    &store_path,
                    &artifacts_root,
                    builder.heartbeat_silence_secs,
                );
                (None, Some(child), Wire::from_stream(stream), None)
            }
        };

        let mut harness = Self {
            _chassis: chassis,
            _coordinator: coordinator,
            _state: owned_state,
            _runs: owned_runs,
            wire,
            fake,
            repo,
            store_path,
            artifacts_root: PathBuf::from(artifacts_root),
            worktree_base,
            base: Digest::default(),
            step_budget: builder.step_budget,
        };

        match builder.backend {
            Backend::Fixture => harness.base = harness.sealable_fixture_base(),
            Backend::LocalRepo if builder.coordinator == CoordinatorKind::InProcess => {
                // Correspondence only. A view() here waits long enough for the
                // land reactor's boot tick to consume a replayed land decision,
                // so a restart scenario would observe Landed before it can
                // assert the journal still reads Resolved.
                harness.wait_for_genesis_correspondence();
            }
            Backend::LocalRepo => harness.base = Snapshot::GENESIS_MAINLINE,
        }

        if builder.auto_seal {
            harness.seal(&builder.workpiece, configs);
        }
        harness
    }

    /// The scratch repository the coordinator runs in, when the backend is a
    /// local working clone.
    ///
    /// # Panics
    /// This is a local-repo cell method and the backend is not a working clone.
    #[must_use]
    pub const fn repo(&self) -> &Repo {
        self.repo.as_ref().expect("repo() is a local-repo cell method")
    }

    /// Where the run directories, script, and ledger live.
    #[must_use]
    pub fn runs_dir(&self) -> PathBuf {
        PathBuf::from(&self.worktree_base)
    }

    /// The whole projection, right now.
    pub fn view(&mut self) -> ViewDocument {
        self.wire.view()
    }

    /// One bloom's view.
    pub fn bloom(&mut self, bloom: BloomId) -> BloomView {
        self.wire.bloom(bloom)
    }

    /// Admit one reducer fact through the control core's wire ingress.
    pub fn admit(&mut self, key: &str, fact: Fact) -> Outcome {
        self.wire.admit(key, fact)
    }

    /// Seal a single-member bloom on the observed mainline and return its id.
    ///
    /// # Panics
    /// The seal was refused.
    pub fn seal_member(&mut self, workpiece: &str, scope_revision: Digest) -> BloomId {
        self.seal_members(&[(workpiece, scope_revision)])
    }

    /// Seal a multi-member bloom on the observed mainline and return its id.
    ///
    /// # Panics
    /// The seal was refused.
    pub fn seal_members(&mut self, members: &[(&str, Digest)]) -> BloomId {
        let base = if self.base == Digest::default() {
            self.view().mainline
        } else {
            self.base
        };
        let spec =
            super::draft(base, &members.iter().map(|(workpiece, scope)| member(workpiece, *scope)).collect::<Vec<_>>());
        let bloom = spec.id();
        let key = members.iter().map(|(workpiece, _)| *workpiece).collect::<Vec<_>>().join("+");
        match self.admit(&format!("fixture-seal-{key}"), Fact::Seal(spec)) {
            Outcome::Sealed(sealed) => assert_eq!(sealed, bloom, "the sealed id is the spec's content address"),
            other => panic!("the fixture seal must seal: {other:?}"),
        }
        bloom
    }

    /// Append a line to the named run's streamed transcript.
    ///
    /// # Panics
    /// The evidence directory could not be created or the file written.
    pub fn write_transcript(&self, nonce: &str, line: &str) {
        let dir = Path::new(&self.worktree_base).join(format!("{nonce}-evidence"));
        fs::create_dir_all(&dir).expect("the evidence directory creates");
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("transcript.jsonl"))
            .expect("the transcript opens")
            .write_all(line.as_bytes())
            .expect("the transcript writes");
    }

    /// Every evidence directory under the run root whose name ends in `-evidence`.
    ///
    /// # Panics
    /// The run root could not be read.
    #[must_use]
    pub fn evidence_nonces(&self) -> Vec<String> {
        fs::read_dir(&self.worktree_base)
            .expect("the run root reads")
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                name.strip_suffix("-evidence").map(str::to_owned)
            })
            .collect()
    }

    /// Poll until the mock ledger holds at least `min` runs.
    ///
    /// # Panics
    /// The budget expired before that many runs were recorded.
    pub fn wait_for_runs(&mut self, min: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.ledger().len() < min {
            assert!(
                Instant::now() < deadline,
                "lane never recorded {min} runs; outstanding={:?} ledger={}",
                self.outstanding(),
                self.ledger().len(),
            );
            thread::sleep(SETTLE_POLL);
        }
    }

    /// Every lane run the mock has recorded, in dispatch order.
    ///
    /// # Panics
    /// The ledger exists but could not be read.
    #[must_use]
    pub fn ledger(&self) -> Vec<LaneRun> {
        read_ledger(Path::new(&self.worktree_base)).expect("the mock ledger reads")
    }

    /// The nonces the store still holds as outstanding orders.
    ///
    /// # Panics
    /// The store could not be opened or read.
    #[must_use]
    pub fn outstanding(&self) -> Vec<String> {
        SqliteStore::open(&self.store_path)
            .expect("the coordinator's journal opens for reading")
            .list_outstanding_nonces()
            .expect("the outstanding-order registry reads")
    }

    /// Poll until `want` holds of the (single) bloom, checking both liveness
    /// invariants on every poll.
    ///
    /// # Panics
    /// The budget expired, or the coordinator went quiescent with work still owed.
    pub fn settle(&mut self, label: &str, want: impl Fn(&BloomView) -> bool) -> BloomView {
        let deadline = Instant::now() + self.step_budget;
        let mut last = None;
        let mut still_since = Instant::now();

        loop {
            let document = self.view();
            if let Some(bloom) = document.blooms.first()
                && want(bloom)
            {
                return bloom.clone();
            }

            let progress = liveness::Progress::observe(&document, self.outstanding(), self.ledger().len());
            let in_flight = !progress.outstanding.is_empty();
            if last.as_ref() != Some(&progress) {
                last = Some(progress);
                still_since = Instant::now();
            } else if !in_flight && still_since.elapsed() >= QUIESCENCE {
                self.judge_quiescence(label, &document);
                panic!(
                    "{label}: the coordinator settled into a legitimate stop without reaching it — {:?}",
                    document.blooms.first().map(|bloom| bloom.status),
                );
            }

            assert!(
                Instant::now() < deadline,
                "{label}: not reached inside {:?}; outstanding={:?} runs={}",
                self.step_budget,
                self.outstanding(),
                self.ledger().len(),
            );
            thread::sleep(SETTLE_POLL);
        }
    }

    /// Assert that the coordinator's current standstill is one it is entitled to.
    ///
    /// # Panics
    /// The standstill is a stall.
    pub fn assert_live(&mut self) {
        let document = self.view();
        self.judge_quiescence("liveness", &document);
    }

    fn judge_quiescence(&self, label: &str, document: &ViewDocument) {
        if let liveness::Quiescence::Stalled(why) = liveness::classify(document, &self.outstanding()) {
            panic!(
                "{label}: the coordinator stopped with work outstanding — {why}. Lane runs recorded: {:?}",
                self.ledger().iter().map(|run| (run.command.clone(), run.mode)).collect::<Vec<_>>(),
            );
        }
    }

    fn seal(&mut self, workpiece: &str, configs: ConfigRegistry) {
        let mut membership = Membership {
            workpiece: WorkpieceId(workpiece.to_owned()),
            scope_revision: Digest::from_bytes([1; 32]),
            configs: ConfigRegistry::default(),
            approval: Evidence {
                subject: Digest::default(),
                kind: EvidenceKind::Approval,
                detail: Digest::from_bytes([200; 32]),
            },
        };
        membership.approval.subject = membership.subject();
        let spec = BloomDraft { proposals: vec![membership], base: self.base, configs, ..BloomDraft::default() }.seal();
        match self.admit("lane-seal", Fact::Seal(spec)) {
            Outcome::Sealed(_) => {}
            other => panic!("the harness seal must seal: {other:?}"),
        }
    }

    fn sealable_fixture_base(&mut self) -> Digest {
        let deadline = Instant::now() + BOOT_BUDGET;
        loop {
            let mainline = self.wire.view().mainline;
            if self
                .fake
                .as_ref()
                .expect("fixture correspondence")
                .resolve_backend_object(&mainline)
                .expect("the fixture correspondence reads")
                .is_some()
            {
                return mainline;
            }
            assert!(Instant::now() < deadline, "the coordinator's mainline never bound to a checkoutable commit");
            thread::sleep(POLL);
        }
    }

    fn wait_for_genesis_correspondence(&self) {
        let deadline = Instant::now() + BOOT_BUDGET;
        loop {
            let bound = SqliteCorrespondence::open(&self.store_path)
                .expect("the correspondence store opens")
                .resolve_backend_object(&Snapshot::GENESIS_MAINLINE)
                .expect("the correspondence store reads")
                .is_some();
            if bound {
                return;
            }
            assert!(Instant::now() < deadline, "the coordinator's mainline never bound to a checkoutable commit");
            thread::sleep(POLL);
        }
    }

    pub(super) fn fake(&self) -> &FakeGithub {
        self.fake.as_ref().expect("this method is a fixture-cell method")
    }
}

fn in_process_env(
    builder: &HarnessBuilder,
    store_path: &str,
    artifacts_root: &str,
    worktree_base: &str,
) -> BloomeryEnv {
    let github = if builder.github_fixture {
        GithubConnectionConfig {
            github_backend: "fixture".to_owned(),
            cas_land_enabled: builder.cas_land_enabled,
            ..GithubConnectionConfig::default()
        }
    } else {
        GithubConnectionConfig { cas_land_enabled: builder.cas_land_enabled, ..GithubConnectionConfig::default() }
    };

    let defaults = CoordinatorConfig::default();
    let scripted = builder.lane == Lane::Scripted;
    let coordinator = CoordinatorConfig {
        store_path: store_path.to_owned(),
        artifacts_root: Some(artifacts_root.to_owned()),
        poll_interval_secs: builder.poll_interval_secs,
        local_lane_enabled: scripted,
        local_lane_commands: if scripted {
            "construct.,review.,verify.".to_owned()
        } else {
            defaults.local_lane_commands
        },
        local_lane_program: if scripted {
            env!("CARGO_BIN_EXE_bloomery-mock-lane").to_owned()
        } else {
            defaults.local_lane_program
        },
        local_worktree_base: if scripted {
            worktree_base.to_owned()
        } else {
            defaults.local_worktree_base
        },
        operator_name: if scripted {
            builder.operator_name.clone()
        } else {
            defaults.operator_name
        },
        operator_email: if scripted {
            builder.operator_email.clone()
        } else {
            defaults.operator_email
        },
        authority_backend: if builder.authority_path.is_some() {
            "local".to_owned()
        } else {
            defaults.authority_backend
        },
        authority_repo: builder
            .authority_path
            .as_ref()
            .map_or(defaults.authority_repo, |path| path.to_string_lossy().into_owned()),
        ..defaults
    };

    BloomeryEnv {
        rpc_port: 0,
        http_port: 0,
        store: StoreConfig { path: store_path.to_owned() },
        artifacts: ArtifactsConfig { root: Some(artifacts_root.to_owned()) },
        github,
        coordinator,
        session: SessionConfig::default(),
        signing: SigningConfig::default(),
    }
}

fn spawn_listening_coordinator(
    repo: &Repo,
    worktree_base: &str,
    store_path: &str,
    artifacts_root: &str,
    heartbeat_silence_secs: Option<u64>,
) -> (Coordinator, TcpStream) {
    let heartbeat = heartbeat_silence_secs.map(|secs| secs.to_string());
    for _ in 0..8 {
        let rpc_port = free_port();
        let mut env = vec![
            ("AETHER_STORE_PATH", store_path),
            ("AETHER_ARTIFACTS_ROOT", artifacts_root),
            ("AETHER_BLOOMERY_LANE_PROGRAM", env!("CARGO_BIN_EXE_bloomery-mock-lane")),
            ("AETHER_GITHUB_LOCAL_WORKTREE_BASE", worktree_base),
            ("AETHER_GITHUB_LOCAL_LANE_COMMANDS", "construct.,review.,verify."),
            ("AETHER_GITHUB_POLL_INTERVAL_SECS", "1"),
            ("AETHER_GITHUB_BACKEND", "fixture"),
            ("AETHER_GITHUB_FIXTURE_BASE_SHA", repo.head()),
            ("AETHER_BLOOMERY_OPERATOR_NAME", "lane harness"),
            ("AETHER_BLOOMERY_OPERATOR_EMAIL", "lane-harness@example.test"),
        ];
        if let Some(secs) = heartbeat.as_deref() {
            env.push(("AETHER_BLOOMERY_HEARTBEAT_SILENCE_SECS", secs));
        }
        let mut coordinator = Coordinator::spawn_in(rpc_port, Some(&repo.work_dir()), &env);
        if !coordinator.is_alive() {
            continue;
        }
        let stream = connect_and_handshake(rpc_port, "lane-boundary-harness");
        if coordinator.is_alive() {
            return (coordinator, stream);
        }
    }
    panic!("the lane coordinator would not stay up long enough to handshake");
}

fn author_catalog(store_path: &str, wall_clock_secs: u64) -> ConfigRegistry {
    let mut catalog = StageCatalog::line();
    for binding in &mut catalog.bindings {
        binding.wall_clock_secs = wall_clock_secs;
    }
    let bytes = to_vec(&catalog).expect("a stage catalog encodes");
    let address = catalog.address();
    SqliteStore::open(store_path)
        .expect("the coordinator's journal opens for writing")
        .record_config(address.as_bytes(), StageCatalog::NAME, &bytes)
        .expect("the authored catalog records");

    let mut configs = ConfigRegistry::default();
    configs.insert::<StageCatalog>(address);
    configs
}

fn backend_object(sha: &str) -> BackendObjectId {
    assert!(sha.len().is_multiple_of(2) && !sha.is_empty(), "git printed a sha that is not whole bytes: {sha}");
    BackendObjectId::new(
        sha.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                u8::from_str_radix(str::from_utf8(pair).expect("a git sha is ASCII"), 16).expect("a git sha is hex")
            })
            .collect(),
    )
}

impl ScenarioHarness {
    /// Wake the executor reactor until the coordinator holds exactly one
    /// outstanding order, and return it.
    pub fn await_order(&mut self) -> OutstandingOrder {
        let mut orders = self.await_orders(1);
        orders.remove(0)
    }

    /// Wake the executor reactor until the coordinator holds exactly `count`
    /// outstanding orders.
    ///
    /// # Panics
    /// The coordinator dispatched more than `count` orders, or nothing inside the step budget.
    pub fn await_orders(&mut self, count: usize) -> Vec<OutstandingOrder> {
        let deadline = Instant::now() + self.step_budget;
        loop {
            self.dispatch_tick();
            let orders = self.orders();
            if orders.len() == count {
                return orders;
            }
            assert!(orders.len() < count, "expected {count} outstanding orders, got {:?}", nonces(&orders));
            assert!(Instant::now() < deadline, "the coordinator dispatched nothing inside {:?}", self.step_budget);
            thread::sleep(POLL);
        }
    }

    /// Arm a cross-member fold collision for `workpiece`.
    pub fn seed_fold_conflict(&self, bloom: BloomId, workpiece: &str, paths: Vec<String>) {
        let hex = short_hex(&bloom.0);
        self.fake().seed_merge_conflict_paths(
            &format!("bloom/{hex}/integration"),
            &format!("bloom/{hex}/candidate/{workpiece}"),
            paths,
        );
    }

    /// Disarm a previously seeded collision.
    pub fn clear_fold_conflict(&self, bloom: BloomId, workpiece: &str) {
        let hex = short_hex(&bloom.0);
        self.fake()
            .clear_merge_conflict(&format!("bloom/{hex}/integration"), &format!("bloom/{hex}/candidate/{workpiece}"));
    }

    /// Persist the work-order description the host threads onto a construct
    /// (or reconcile) dispatch.
    ///
    /// # Panics
    /// The journal could not be opened or the description could not be written.
    pub fn record_description(&self, bloom: BloomId, workpiece: &str, description: &str) {
        SqliteStore::open(&self.store_path)
            .expect("the coordinator's journal opens for writing")
            .record_dispatch_description(bloom.0.as_bytes(), workpiece, description)
            .expect("the work-order description persists");
    }

    /// Wake the land reactor until `bloom` reaches `want`.
    ///
    /// # Panics
    /// The bloom did not reach `want` inside the step budget.
    pub fn await_landing(&mut self, bloom: BloomId, want: BloomStatus) {
        let deadline = Instant::now() + self.step_budget;
        loop {
            self.land_tick();
            let status = self.bloom(bloom).status;
            if status == want {
                return;
            }
            assert!(Instant::now() < deadline, "the bloom stayed {status:?} rather than reaching {want:?}");
            thread::sleep(POLL);
        }
    }

    /// Same wait as [`await_landing`](Self::await_landing), named for the
    /// local-authority cell.
    pub fn land_until(&mut self, bloom: BloomId, want: BloomStatus) {
        self.await_landing(bloom, want);
    }

    /// Wake the executor, integrate, and observe until `pred` holds.
    ///
    /// # Panics
    /// `pred` did not hold inside the step budget.
    pub fn pump_until(&mut self, what: &str, pred: impl Fn(&mut Self) -> bool) {
        let deadline = Instant::now() + self.step_budget;
        loop {
            self.dispatch_tick();
            self.integrate_tick();
            if pred(self) {
                return;
            }
            if Instant::now() >= deadline {
                let view = self.bloom_debug();
                let orders = self.orders();
                panic!("{what} did not happen inside {:?}; {view}; outstanding={orders:?}", self.step_budget);
            }
            thread::sleep(POLL);
        }
    }

    fn bloom_debug(&mut self) -> String {
        let document = self.view();
        let blooms: Vec<String> = document
            .blooms
            .iter()
            .map(|bloom| {
                format!(
                    "bloom status={:?} landing={:?} fault={:?} park={:?} composition={:?} members={:?}",
                    bloom.status,
                    bloom.landing_blocked,
                    bloom.executor_fault,
                    bloom.review_park,
                    bloom.composition,
                    bloom
                        .members
                        .iter()
                        .map(|member| (member.workpiece.0.clone(), member.resolution.is_some(), member.wedge.is_some()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        format!("mainline={:?} blooms={blooms:?}", document.mainline)
    }

    /// Wake the executor reactor once.
    pub fn dispatch_tick(&mut self) {
        self.wire.tick(<ExecutorReactorCapability as Addressable>::resolve(0, ()), &DispatchTick::default());
    }

    /// Wake the integrate reactor once.
    pub fn integrate_tick(&mut self) {
        self.wire.tick(<IntegrateReactorCapability as Addressable>::resolve(0, ()), &IntegrateTick::default());
    }

    /// Wake the land reactor once.
    pub fn land_tick(&mut self) {
        self.wire.tick(<LandReactorCapability as Addressable>::resolve(0, ()), &LandTick::default());
    }

    /// Wake the control core's mainline observer once.
    pub fn observe_tick(&mut self) {
        self.wire.tick(control_mailbox(), &ObserveTick::default());
    }

    /// Move the repository's mainline to `head` as a fast-forward.
    pub fn move_mainline(&self, head: Digest) {
        const MAINLINE_REF: &str = "heads/main";
        self.fake().seed_fast_forward(&head, self.fake().ref_target(MAINLINE_REF).as_deref());
        self.fake().seed_ref_at(MAINLINE_REF, &head);
    }

    /// Point the repository's mainline at `head` with no ancestry from the
    /// current tip — a history rewrite of the ref.
    pub fn rewrite_mainline(&self, head: Digest) {
        const MAINLINE_REF: &str = "heads/main";
        self.fake().seed_fast_forward(&head, None);
        self.fake().seed_ref_at(MAINLINE_REF, &head);
    }

    /// Wake the observer until the coordinator's mainline reads `want`.
    ///
    /// # Panics
    /// Mainline did not reach `want` inside the step budget.
    pub fn await_mainline(&mut self, want: Digest) {
        let deadline = Instant::now() + self.step_budget;
        loop {
            self.observe_tick();
            let mainline = self.view().mainline;
            if mainline == want {
                return;
            }
            assert!(Instant::now() < deadline, "mainline stayed {mainline:?} rather than reaching {want:?}");
            thread::sleep(POLL);
        }
    }

    /// Carry a bloom whose claim set is complete through the fold, its
    /// aggregate gates, and the landing.
    pub fn land_the_fold(&mut self, bloom: BloomId) -> bool {
        let (mechanical_ran, _proposal) = self.resolve_and_propose(bloom);
        self.await_landing(bloom, BloomStatus::Landed);
        mechanical_ran
    }

    /// The same tail up to the landing proposal being open.
    ///
    /// # Panics
    /// The dispatched order was not bloom-level, the critic's gate did not run, or no landing was proposed.
    pub fn resolve_and_propose(&mut self, bloom: BloomId) -> (bool, u64) {
        self.integrate_tick();

        let order = self.await_order();
        assert!(order.workpiece.is_empty(), "a bloom-level order carries no member axis");
        let mut key = self.upload_admitted(&super::passed(&order));

        let mechanical_ran = key.starts_with("aether.bloomery.aggregate_verify:");
        if mechanical_ran {
            let aggregate_review = self.await_order();
            key = self.upload_admitted(&super::passed(&aggregate_review));
        }
        assert!(key.starts_with("aether.bloomery.aggregate_review:"), "the critic's gate: {key}");

        self.land_tick();
        let proposal = self.landing_proposal(bloom).expect("a resolved bloom proposes a landing");
        (mechanical_ran, proposal)
    }

    /// Whether landing proposal `number` has merged.
    ///
    /// # Panics
    /// The fixture does not hold the proposal.
    #[must_use]
    pub fn landing_merged(&self, number: u64) -> bool {
        self.fake().pull_request_merged(number).expect("the fixture holds the proposal")
    }

    /// Every order the coordinator currently holds outstanding.
    ///
    /// # Panics
    /// The journal could not be opened or an outstanding nonce did not resolve.
    #[must_use]
    pub fn orders(&self) -> Vec<OutstandingOrder> {
        let mut store = SqliteStore::open(&self.store_path).expect("the coordinator's journal opens for reading");
        store
            .list_outstanding_nonces()
            .expect("the outstanding-order registry reads")
            .into_iter()
            .filter_map(|nonce| store.lookup_order(&nonce).expect("a listed nonce resolves to its order"))
            .collect()
    }

    /// Upload one scripted verdict against an order the coordinator dispatched.
    ///
    /// # Panics
    /// The scripted upload could not be encoded.
    pub fn upload(&mut self, upload: &ScriptedUpload) -> ScriptedEvidenceResult {
        let evidence = ScriptedEvidence { upload: to_vec(upload).expect("a scripted upload encodes") };
        let mailbox = <ExecutorReactorCapability as Addressable>::resolve(0, ());
        self.wire.call(mailbox, &evidence)
    }

    /// Upload a scripted verdict and assert the broker admitted it.
    ///
    /// # Panics
    /// The broker did not admit the verdict.
    pub fn upload_admitted(&mut self, upload: &ScriptedUpload) -> String {
        match self.upload(upload) {
            ScriptedEvidenceResult::Admitted { idempotency_key } => idempotency_key,
            other => panic!("the scripted verdict was not admitted: {other:?}"),
        }
    }

    /// Stage the capture a construct lane would have produced.
    ///
    /// # Panics
    /// The fixture could not mint the capture commit.
    #[must_use]
    pub fn seed_capture(&self, bloom: BloomId, workpiece: &str, tree: Digest, checkout: Digest) -> CandidateRef {
        let tree_sha = to_hex(&tree);
        let commit = self
            .fake()
            .create_commit(&format!("capture {workpiece}"), &tree_sha, &[])
            .expect("the fixture mints the capture commit");

        self.fake().seed_ref(candidate_ref_name(&bloom, workpiece).trim_start_matches("refs/"), &commit.sha);
        self.fake().seed_correspondence(&tree, &tree_sha);
        self.fake().seed_correspondence(&checkout, &commit.sha);
        CandidateRef { tree, checkout }
    }

    /// The study artifact the executor reactor filed for `(bloom, attempt)`.
    ///
    /// # Panics
    /// The journal could not be opened or the study index could not be read.
    #[must_use]
    pub fn study_index_row(&self, bloom: BloomId, attempt: Digest) -> Option<String> {
        SqliteStore::open(&self.store_path)
            .expect("the coordinator's journal opens for reading")
            .lookup_study(bloom.0.as_bytes(), attempt.as_bytes())
            .expect("the study index reads")
    }

    /// Fetch one artifact from the store root the chassis was configured with.
    ///
    /// # Panics
    /// The configured artifacts root could not be opened.
    #[must_use]
    pub fn artifact(&self, digest: &str) -> GetResult {
        ArtifactsCapabilityState::open(&self.artifacts_root)
            .expect("the configured artifacts root opens")
            .get(digest.to_owned())
    }

    /// The number of the landing proposal open for `bloom`.
    ///
    /// # Panics
    /// The fixture pull-request surface did not answer.
    #[must_use]
    pub fn landing_proposal(&self, bloom: BloomId) -> Option<u64> {
        self.fake()
            .find_pull_request_for_head(&landing_branch(&bloom))
            .expect("the fixture pull-request surface answers")
            .map(|pull| pull.number)
    }
}

fn nonces(orders: &[OutstandingOrder]) -> Vec<&str> {
    orders.iter().map(|order| order.nonce.as_str()).collect()
}
