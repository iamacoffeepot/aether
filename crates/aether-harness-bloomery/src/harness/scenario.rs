//! Boot construction for [`ScenarioHarness`]: env, coordinator, handshake, wait.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aether_actor::Addressable;
use aether_bloomery::{
    BackendObjectId, BloomDraft, BloomId, BloomSpec, BloomStatus, BloomView, CandidateRef, ConfigKind, ConfigRegistry,
    Correspondence, Digest, Evidence, EvidenceKind, Fact, FakeKeyProvider, KeyId, MemberDependency, Membership,
    Observation, Outcome, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, Snapshot, StageCatalog,
    StageId, Statement, VerifyFailureSet, ViewDocument, WorkpieceId, signed_approval,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitDataApi, PullRequestApi, candidate_ref_name, landing_branch, short_hex, to_hex};
use aether_chassis_bloomery::artifacts::{ArtifactsCapabilityState, ArtifactsConfig, GetResult};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneMode, LaneRun, LaneScript as MockLaneScript, read_ledger};
use aether_chassis_bloomery::bloomery::{
    BloomeryChassis, BloomeryEnv, Chassis, CoordinatorConfig, DispatchTick, DoctorReactorCapability, DoctorReport,
    DoctorTick, ExecutorReactorCapability, GithubConnectionConfig, IntegrateReactorCapability, IntegrateTick,
    JanitorReactorCapability, JanitorTick, LandReactorCapability, LandTick, NotifyConfig, ScriptedEvidence,
    ScriptedEvidenceResult, ScriptedUpload,
};
use aether_chassis_bloomery::commission::task_text;
use aether_chassis_bloomery::control::ObserveTick;
use aether_chassis_bloomery::session::SessionConfig;
use aether_chassis_bloomery::signing::SigningConfig;
use aether_chassis_bloomery::store::{
    CommissionBackend, OutstandingOrder, RevisionEvidence, SqliteCorrespondence, SqliteStore, StoreBackend, StoreConfig,
};
use aether_data::Kind;
use aether_data::wire::{from_bytes, to_vec};
use aether_http::HttpServerHandle;
use aether_rpc::RpcServerHandle;
use aether_substrate::chassis::builder::BuiltChassis;
use tempfile::TempDir;

use super::digest;
use super::drive::{member, passed};
use super::{BOOT_BUDGET, Backend, CoordinatorKind, HARNESS_STARTED, HarnessBuilder, Lane, POLL};
use crate::oracle::{Oracle, is_answerable, liveness};
use crate::scenario::{LaneScript, Scenario};
use crate::support::Coordinator;
use crate::support::client::spawn_and_connect;
use crate::support::repo::Repo;
use crate::support::wire::{Wire, control_mailbox};

/// How long the world must hold still, with nothing in flight, before a settle
/// loop calls it quiescent. Comfortably more than the poll cadence plus the gap
/// between consuming one order and dispatching the next.
const QUIESCENCE: Duration = Duration::from_secs(12);

/// Between polls of a forked coordinator's projection.
const SETTLE_POLL: Duration = Duration::from_millis(250);

/// How long a forked coordinator has to come up and answer a handshake, across
/// however many forks that takes. Generous, because a loaded scenario suite
/// boots many at once; a child that dies is retried immediately rather than
/// waited out, so this is a ceiling and not a cost.
const COORDINATOR_HANDSHAKE_BUDGET: Duration = Duration::from_mins(1);

/// The signing seed the harness's operator answers a surface request with.
///
/// A fixture, like every other key in a test tree: what the scenario needs is
/// a statement whose provenance is an author signature rather than the
/// estate's own observation, and the bytes behind it are nobody's secret.
const OPERATOR_SEED: [u8; 32] = [0x0A; 32];

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
    /// The spec of the last bloom sealed through this harness.
    ///
    /// Held because an amendment is a supersession (see
    /// [`ScenarioHarness::apply_operator`]) and a successor has to carry the
    /// predecessor's members across at the revisions they already hold — which
    /// the projection does not report, and which only the sealer knows.
    pub(super) sealed: Option<BloomSpec>,
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

        let BootRoots { owned_state, owned_runs, store_path, artifacts_root, worktree_base } = boot_roots(&builder);

        if let Some(script) = &builder.script {
            script.write_to(Path::new(&worktree_base)).expect("the mock-lane script writes");
        }

        let repo = match builder.backend {
            Backend::Fixture => None,
            Backend::LocalRepo if builder.authority_path.is_some() => builder.repo.take(),
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
                let mut wire = Wire::connect(port, client_name);
                if let Some(timeout) = builder.socket_read_timeout {
                    wire.set_read_timeout(timeout);
                }
                if let Some(http) = chassis.handle::<HttpServerHandle>() {
                    wire.set_http_port(http.local_port);
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
            sealed: None,
            step_budget: builder.step_budget,
        };

        // The control core refuses every read until its boot journal replay has
        // folded, so a scenario step that lands in that window reads a refusal
        // where it expected a projection. Awaited once, here, rather than
        // retried at each call site: the flag never goes back, so the window
        // this closes is the only one a scenario can meet.
        harness.wire.await_replayed();

        match builder.backend {
            Backend::Fixture => harness.base = harness.sealable_fixture_base(),
            Backend::LocalRepo if builder.coordinator == CoordinatorKind::InProcess => {
                // Correspondence only, and no further wait: the land reactor's
                // boot tick consumes a replayed land decision, so a restart
                // scenario that idles here would observe Landed before it can
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

    /// The doctor's latest pass, as `GET /view` overlays it.
    pub fn doctor(&mut self) -> Option<DoctorReport> {
        self.wire.doctor()
    }

    /// One bloom's view.
    pub fn bloom(&mut self, bloom: BloomId) -> BloomView {
        self.wire.bloom(bloom)
    }

    /// Admit one reducer fact through the control core's wire ingress.
    pub fn admit(&mut self, key: &str, fact: Fact) -> Outcome {
        self.wire.admit(key, fact)
    }

    /// Local-authority cell over the three-crate example project.
    #[must_use]
    pub fn start() -> Self {
        let repo = Repo::with_example_project();
        HarnessBuilder::local_authority(&repo).hold_repo(repo).start("bloomery-harness")
    }

    /// Seal `scenario`'s members on the observed mainline.
    ///
    /// # Panics
    /// The seal was refused, or the scenario named more members than a digest seed can index.
    pub fn seal_scenario(&mut self, scenario: &Scenario) -> BloomId {
        let members: Vec<(&str, Digest)> = scenario
            .members
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                (spec.workpiece.0.as_str(), digest(u8::try_from(index + 1).expect("a scenario has at most 4 members")))
            })
            .collect();
        let pairs: Vec<(String, Digest)> =
            members.iter().map(|(workpiece, digest)| ((*workpiece).to_owned(), *digest)).collect();
        let refs: Vec<(&str, Digest)> = pairs.iter().map(|(workpiece, digest)| (workpiece.as_str(), *digest)).collect();
        self.seal_members(&refs)
    }

    /// Write `scripts` for `workpiece`'s `stage` as a mock-lane script.
    ///
    /// # Panics
    /// The mock-lane script could not be written.
    pub fn script_lane(&self, workpiece: &WorkpieceId, stage: StageId, scripts: &[LaneScript]) {
        let command = stage_command(stage);
        let mut script = MockLaneScript::all_passing();
        for item in scripts {
            script = script.then(command, lower_lane_script(item));
        }
        // BaseVerify is bloom-less: the reserved empty workpiece is the order's
        // member axis, same as aggregate verify.
        let _ = workpiece;
        script.write_to(Path::new(&self.worktree_base)).expect("the mock-lane script writes");
    }

    /// The served red-base alert, when one is holding the day.
    #[must_use]
    pub fn base_receipt(&mut self) -> Option<aether_bloomery::BaseAlertView> {
        self.view().base_alert
    }

    /// Tick until `predicate` holds or `ticks` is exhausted, checking
    /// [`Oracle`] only when the world has gone still with nothing in flight.
    ///
    /// # Panics
    /// The oracle objected, or `ticks` elapsed before the predicate held.
    pub fn run_until(&mut self, predicate: impl Fn(&mut Self) -> bool, ticks: u32) {
        let mut last: Option<liveness::Progress> = None;
        let mut still = 0_u32;
        for _ in 0..ticks {
            self.tick();
            let progress = liveness::Progress::observe(&self.view(), self.outstanding(), self.ledger().len());
            if last.as_ref() == Some(&progress) {
                still += 1;
            } else {
                last = Some(progress.clone());
                still = 0;
            }
            if still >= 2 && is_answerable(&progress) {
                self.check_oracle("");
            }
            if predicate(self) {
                let progress = liveness::Progress::observe(&self.view(), self.outstanding(), self.ledger().len());
                if is_answerable(&progress) {
                    self.check_oracle("");
                }
                return;
            }
        }
        self.check_oracle("tick budget exhausted: ");
        panic!("predicate not reached inside {ticks} ticks");
    }

    /// Check the [`Oracle`] against a report of the world as it stands now.
    ///
    /// The doctor writes its report on its own tick, so reading the report from
    /// before the last admission judges a world that has since moved on: a
    /// member whose park landed between that report and this read still reads
    /// as a member with no lane and no dispatch. Re-ticking the doctor first is
    /// what makes the report and the document one instant rather than two.
    ///
    /// # Panics
    /// The oracle objected.
    fn check_oracle(&mut self, context: &str) {
        self.doctor_tick();
        Oracle::check(&self.view(), self.doctor().as_ref(), &self.outstanding())
            .unwrap_or_else(|violation| panic!("{context}{violation}"));
    }

    /// Author `workpiece`'s commission and freeze one scope revision declaring
    /// `surface`, returning the revision's digest — the value a member is then
    /// sealed at.
    ///
    /// Most scenarios seal a member at a bare digest, because nothing they
    /// exercise reads the revision behind it. A scenario about a surface
    /// *amendment* needs the real record: the operator loads the current
    /// revision, widens its declared surface, and writes the successor back
    /// through the same commission store, so a member sealed at a digest no
    /// revision stands behind can only ever park.
    ///
    /// # Panics
    /// The commission store could not be opened, or the commission and its
    /// first revision could not be written.
    #[must_use]
    pub fn author_scope_revision(&self, workpiece: &str, surface: &[&str]) -> Digest {
        let mut store = SqliteStore::open(&self.store_path).expect("the commission store opens for writing");
        let workpiece = WorkpieceId(workpiece.to_owned());
        let intent = Statement {
            words: format!("scope {}", workpiece.0).into_bytes(),
            provenance: Provenance::ObservationAttestation(Observation { source: String::from("bloomery harness") }),
            parents: Vec::new(),
        };
        store.create(&workpiece, &intent).expect("the commission is created");

        let revision = ScopeRevision {
            schema: SCOPE_REVISION_SCHEMA,
            workpiece,
            predecessor: None,
            problem: String::from("the harness authored this scope"),
            design: String::new(),
            plan: String::new(),
            declared_surface: surface.iter().map(|glob| (*glob).to_owned()).collect(),
            dogfood_brief: String::new(),
            routing: ScopeRouting { size: String::from("S"), model: String::new() },
            dependencies: Vec::new(),
            description: String::new(),
            implements: Vec::new(),
            declared_crates: Vec::new(),
            declared_reads: Vec::new(),
        };

        // Rendered rather than left empty, because the work order a lane reads
        // is this text: the seal door renders it once and the dispatch replays
        // it, so a revision with no description produces a subject-only prompt
        // that states no surface at all.
        let revision = ScopeRevision { description: task_text(&revision), ..revision };
        store.write_revision(&revision, &RevisionEvidence::default()).expect("the scope revision writes")
    }

    /// The scope revision the commission store holds under `digest`, or `None`
    /// when it holds none.
    ///
    /// # Panics
    /// The commission store could not be opened or read.
    #[must_use]
    pub fn scope_revision(&self, digest: Digest) -> Option<ScopeRevision> {
        SqliteStore::open(&self.store_path)
            .expect("the commission store opens for reading")
            .load_revision(digest)
            .expect("the commission store reads")
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
        let (bloom, outcome) = self.try_seal(members);
        match outcome {
            Outcome::Sealed(sealed) => assert_eq!(sealed, bloom, "the sealed id is the spec's content address"),
            other => panic!("the fixture seal must seal: {other:?}"),
        }
        self.pass_outstanding_base_verify();
        bloom
    }

    /// Seal a multi-member bloom carrying a declared dependency graph.
    ///
    /// The edgeless [`Fact::Seal`] is what `seal_members` admits, and a
    /// scenario about readiness, splices, or a cascading withdrawal needs the
    /// edges the door would have resolved from the members' scopes. `edges` is
    /// `(member, depends_on)` pairs, both member workpieces.
    ///
    /// # Panics
    /// The seal was refused.
    pub fn seal_graph(&mut self, members: &[(&str, Digest)], edges: &[(&str, &str)]) -> BloomId {
        let base = if self.base == Digest::default() {
            self.view().mainline
        } else {
            self.base
        };
        let spec =
            super::draft(base, &members.iter().map(|(workpiece, scope)| member(workpiece, *scope)).collect::<Vec<_>>());
        let bloom = spec.id();
        let edges = edges
            .iter()
            .map(|(dependent, depends_on)| MemberDependency {
                member: WorkpieceId((*dependent).to_owned()),
                depends_on: WorkpieceId((*depends_on).to_owned()),
            })
            .collect::<Vec<_>>();
        let key = members.iter().map(|(workpiece, _)| *workpiece).collect::<Vec<_>>().join("+");
        match self.admit(
            &format!("fixture-graph-seal-{key}"),
            Fact::GraphSeal { predecessor: None, spec: spec.clone(), edges },
        ) {
            Outcome::Sealed(sealed) => assert_eq!(sealed, bloom, "the sealed id is the spec's content address"),
            other => panic!("the fixture graph seal must seal: {other:?}"),
        }
        self.sealed = Some(spec);
        self.pass_outstanding_base_verify();

        bloom
    }

    /// Attempt the same seal [`seal_members`](Self::seal_members) makes, and
    /// hand back the spec's id alongside whatever the door answered.
    ///
    /// The id is returned even on a refusal: it is the content address of the
    /// spec that was offered, so a scenario asserting a refusal can still say
    /// which bloom was refused and check that nothing by that id exists.
    ///
    /// Kept beside the asserting form rather than replacing it, because a
    /// scenario that seals as a *precondition* wants the panic — a fixture seal
    /// that quietly failed would surface later as an unrelated missing bloom.
    pub fn try_seal(&mut self, members: &[(&str, Digest)]) -> (BloomId, Outcome) {
        let base = if self.base == Digest::default() {
            self.view().mainline
        } else {
            self.base
        };
        let spec =
            super::draft(base, &members.iter().map(|(workpiece, scope)| member(workpiece, *scope)).collect::<Vec<_>>());
        let bloom = spec.id();
        let key = members.iter().map(|(workpiece, _)| *workpiece).collect::<Vec<_>>().join("+");
        let outcome = self.admit(&format!("fixture-seal-{key}"), Fact::Seal(spec.clone()));
        // Remembered only on success: a refused spec never became this
        // harness's bloom, and an amendment against it would supersede
        // something that does not exist.
        if matches!(outcome, Outcome::Sealed(_)) {
            self.persist_work_orders(&spec);
            self.sealed = Some(spec);
        }

        (bloom, outcome)
    }

    /// Persist each member's work order the way the seal and supersede doors do.
    ///
    /// `admit_member` renders the admitted revision through `task_text`, and the
    /// door writes the result to the dispatch-description row every construct
    /// prompt is assembled from — keyed by the sealed bloom's id, so a
    /// supersession mints its own rows rather than inheriting the
    /// predecessor's. A scenario that seals at a bare digest has no revision
    /// behind it to render, and keeps the subject-only prompt it had.
    ///
    /// # Panics
    /// The store could not be opened, or a row could not be written.
    pub(super) fn persist_work_orders(&self, spec: &BloomSpec) {
        let mut store = SqliteStore::open(&self.store_path).expect("the store opens for writing");
        for member in spec.members() {
            let Ok(Some(revision)) = store.load_revision(member.scope_revision) else {
                continue;
            };
            store
                .record_dispatch_description(spec.id().0.as_bytes(), &member.workpiece.0, &task_text(&revision))
                .expect("the member's work order records");
        }
    }

    /// Write `widened` as the commission's next revision and store the
    /// operator's approval of it — the two store writes `cargo xtask bloom
    /// amend` performs before it supersedes (ADR-0207).
    ///
    /// The approval carries an author signature rather than an observation
    /// attestation, because that is what a widening is: an operator's decision,
    /// signed with a key the coordinator does not hold. Custody here is the
    /// harness's, so the seed is a fixture and the verifier is the stub one.
    ///
    /// # Panics
    /// The store could not be opened, or the revision and its approval could
    /// not be written.
    #[must_use]
    pub fn approve_widened_revision(&self, widened: &ScopeRevision) -> Digest {
        let mut store = SqliteStore::open(&self.store_path).expect("the commission store opens for writing");
        let revision = store.write_revision(widened, &RevisionEvidence::default()).expect("the successor writes");
        store
            .insert_approval(
                &signed_approval(KeyId(String::from("operator")), &OPERATOR_SEED, revision),
                &FakeKeyProvider,
            )
            .expect("the operator's approval stores");
        revision
    }

    /// Pass a queued `verify.base` so a scenario that is not about base
    /// admission sees construct orders the way it did before the gate.
    fn pass_outstanding_base_verify(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.dispatch_tick();
            let orders = self.orders();
            if let Some(order) = orders
                .iter()
                .find(|order| from_bytes::<StageId>(&order.stage).is_ok_and(|stage| stage == StageId::BaseVerify))
            {
                self.upload_admitted(&passed(order));
                return;
            }
            if !orders.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(POLL);
        }
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

    /// Truncate-write the named run's lane heartbeat file.
    ///
    /// The coordinator reads this file's modification time as liveness once the
    /// streamed transcript has gone quiet. Truncating is the signal: the body
    /// is not read.
    ///
    /// # Panics
    /// The evidence directory could not be created or the file written.
    pub fn touch_heartbeat(&self, nonce: &str) {
        let dir = Path::new(&self.worktree_base).join(format!("{nonce}-evidence"));
        fs::create_dir_all(&dir).expect("the evidence directory creates");
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("now is after the epoch").as_millis();
        fs::write(dir.join("heartbeat"), stamp.to_string()).expect("the heartbeat writes");
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
            if last.as_ref() != Some(&progress) {
                last = Some(progress);
                still_since = Instant::now();
            } else if is_answerable(&progress) && still_since.elapsed() >= QUIESCENCE {
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
        self.prove_base(self.base);
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

    /// Stamp a green whole-workspace receipt for `base` so an auto-seal
    /// dispatches construct without running `verify.base` through the mock
    /// lane. Scenarios that are *about* base admission use [`Self::try_seal`].
    fn prove_base(&mut self, base: Digest) {
        match self.admit(
            "fixture-base-verify",
            Fact::BaseVerifyCompleted {
                base,
                tree: base,
                passed: true,
                evidence: Evidence {
                    subject: base,
                    kind: EvidenceKind::VerificationResult,
                    detail: Digest::from_bytes([9; 32]),
                },
                failed: VerifyFailureSet::EMPTY,
            },
        ) {
            Outcome::BaseProven { .. } => {}
            other => panic!("the fixture base must prove green: {other:?}"),
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

/// Where one booting harness keeps its journal, artifacts, and lane worktrees,
/// and which of those directories it owns.
struct BootRoots {
    /// The journal / artifacts tempdir, when this harness minted it. `None` on
    /// shared roots, whose lifetime belongs to the [`HarnessRoots`] a restart
    /// scenario holds across both coordinators.
    ///
    /// [`HarnessRoots`]: super::HarnessRoots
    owned_state: Option<TempDir>,
    /// The lane-worktree tempdir, on the same terms.
    owned_runs: Option<TempDir>,
    store_path: String,
    artifacts_root: String,
    worktree_base: String,
}

/// Fresh temporary roots, or the shared ones a restart scenario passed in.
fn boot_roots(builder: &HarnessBuilder) -> BootRoots {
    if let (Some(store), Some(artifacts), Some(worktree)) =
        (&builder.shared_store, &builder.shared_artifacts, &builder.shared_worktree)
    {
        return BootRoots {
            owned_state: None,
            owned_runs: None,
            store_path: store.clone(),
            artifacts_root: artifacts.clone(),
            worktree_base: worktree.clone(),
        };
    }

    let state = tempfile::tempdir().expect("a temporary root for the journal and the artifacts store");
    let runs = tempfile::tempdir().expect("lane worktree base");
    BootRoots {
        store_path: state.path().join("bloomery.db").to_string_lossy().into_owned(),
        artifacts_root: state.path().join("artifacts").to_string_lossy().into_owned(),
        worktree_base: runs.path().to_string_lossy().into_owned(),
        owned_state: Some(state),
        owned_runs: Some(runs),
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
            crate::mock_lane_program()
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
        // No webhook path, so the notification reactor mounts disabled (#5166):
        // a scenario asserts on the coordinator's own transitions, and a
        // scripted run has no operator channel to shout down.
        notify: NotifyConfig::default(),
        coordinator,
        session: SessionConfig::default(),
        signing: SigningConfig::default(),
    }
}

/// Fork the lane coordinator inside `repo` and handshake the child that stayed
/// up, retrying the whole fork rather than the connect.
///
/// RPC port `0`: the child holds its port from the moment it binds and reports
/// which one in its boot log, so a concurrently booting sibling has no window in
/// which to take it.
fn spawn_listening_coordinator(
    repo: &Repo,
    worktree_base: &str,
    store_path: &str,
    artifacts_root: &str,
    heartbeat_silence_secs: Option<u64>,
) -> (Coordinator, TcpStream) {
    let heartbeat = heartbeat_silence_secs.map(|secs| secs.to_string());
    let lane_program = crate::mock_lane_program();
    spawn_and_connect("lane-boundary-harness", COORDINATOR_HANDSHAKE_BUDGET, || {
        let mut env = vec![
            ("AETHER_STORE_PATH", store_path),
            ("AETHER_ARTIFACTS_ROOT", artifacts_root),
            ("AETHER_BLOOMERY_LANE_PROGRAM", lane_program.as_str()),
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
        Coordinator::spawn_in(0, Some(&repo.work_dir()), &env)
    })
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
    BackendObjectId::new(aether_bloomery::decode_hex(sha).expect("a git sha is lowercase hex"))
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

    /// Wake the doctor reactor once so `/view` overlays a fresh report.
    pub fn doctor_tick(&mut self) {
        self.wire.tick(<DoctorReactorCapability as Addressable>::resolve(0, ()), &DoctorTick::default());
    }

    /// Wake the janitor reactor once so a scenario can observe a reclaim pass
    /// without waiting on its poll timer.
    ///
    /// Kept off [`Self::tick`]: a scenario that is not about retention must
    /// not start reclaiming session trees mid-walk just because it advanced
    /// the other reactors.
    pub fn janitor_tick(&mut self) {
        self.wire.tick(<JanitorReactorCapability as Addressable>::resolve(0, ()), &JanitorTick::default());
    }

    /// One round of executor / integrate / land / observe / doctor.
    pub fn tick(&mut self) {
        self.dispatch_tick();
        self.integrate_tick();
        self.land_tick();
        self.observe_tick();
        self.doctor_tick();
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
    pub fn land_the_fold(&mut self, bloom: BloomId) {
        self.resolve_and_propose(bloom);
        self.await_landing(bloom, BloomStatus::Landed);
    }

    /// The same tail up to the landing proposal being open, returning the
    /// landing proposal's number.
    ///
    /// # Panics
    /// The dispatched order was not bloom-level, either aggregate gate did not run, or no landing was proposed.
    pub fn resolve_and_propose(&mut self, bloom: BloomId) -> u64 {
        self.integrate_tick();

        // Both composite gates go out over the same fold and neither reads the
        // other's verdict, so both orders stand outstanding at once and either
        // can arrive first. Both run over every fold, a fold of one live member
        // included: the member position does not run `verify.docs`, so its proof
        // answers a narrower question than the fold's and the memo does not hit.
        let orders = self.await_orders(2);
        let mut keys = Vec::new();
        for order in &orders {
            assert!(order.workpiece.is_empty(), "a bloom-level order carries no member axis");
            keys.push(self.upload_admitted(&passed(order)));
        }
        for gate in ["aether.bloomery.aggregate_review:", "aether.bloomery.aggregate_verify:"] {
            assert!(keys.iter().any(|key| key.starts_with(gate)), "the {gate} gate ran: {keys:?}");
        }

        self.land_tick();
        self.landing_proposal(bloom).expect("a resolved bloom proposes a landing")
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

fn stage_command(stage: StageId) -> &'static str {
    match stage {
        StageId::Verify => aether_bloomery::VERIFY_MEMBER_COMMAND,
        StageId::AggregateVerify => aether_bloomery::VERIFY_CHECK_COMMAND,
        StageId::BaseVerify => aether_bloomery::VERIFY_BASE_COMMAND,
        StageId::AggregateReview => aether_bloomery::REVIEW_CRITIC_COMMAND,
        _ => aether_bloomery::CONSTRUCT_IMPLEMENT_COMMAND,
    }
}

fn lower_lane_script(script: &LaneScript) -> LaneMode {
    match script {
        LaneScript::Candidate => LaneMode::Pass,
        LaneScript::Decline => LaneMode::Declines,
        LaneScript::DeclineRequestingSurface => LaneMode::DeclinesRequestingSurface,
        LaneScript::OutsideSurface(_) | LaneScript::VerifyFail(_) | LaneScript::BaseVerifyFail(_) => LaneMode::Fail,
        LaneScript::Die => LaneMode::ExitsNonZero,
        LaneScript::WrongSubject => LaneMode::WrongSubject,
    }
}
