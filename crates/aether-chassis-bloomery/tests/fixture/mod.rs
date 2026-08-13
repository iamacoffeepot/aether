//! The in-process fixture harness (#4711): a real coordinator chassis, booted
//! in the test process against temporary stores and an in-memory GitHub, driven
//! one explicit tick at a time.
//!
//! # What stays real
//!
//! Every outbox row's producer and consumer. The reducer decides, the control
//! core commits its decisions into the store's outbox topics, and the
//! boot-constructed reactors drain those exact rows — no scenario ever places
//! one. That is the whole point: the reactor-to-reactor handoff is the thing
//! this tier tests, and a fixture that enqueued the row it claims to prove would
//! test the enqueue and nothing else.
//!
//! # What is substituted
//!
//! Two things, not one.
//!
//! The **verdict** a model would have produced arrives through
//! [`ScriptedEvidence`](aether_chassis_bloomery::bloomery::ScriptedEvidence),
//! which the executor reactor admits through its own outstanding-order registry
//! — so a scenario can only answer an order the coordinator really dispatched,
//! bound to the digest that order really displayed.
//!
//! The **candidate push** is substituted too, and less visibly. Production's
//! pull path ends by resolving an admitted capture's checkout through
//! correspondence and force-pushing that commit to the bloom's candidate ref
//! (ADR-0152); the scripted admit path omits that step, and
//! [`seed_capture`](FixtureHarness::seed_capture) plants the same ref itself
//! through the same `candidate_ref_name` helper. The omission is forced — the
//! production pusher shells a real `git push --force origin` — but the cost is
//! real: a wrong ref name, a dropped push, or a mis-resolved correspondence is
//! invisible here, because the fold reads a ref this harness wrote. That step
//! belongs to the lane-boundary tier, which runs a real pusher.
//!
//! # Warning: do not seed a completed run here
//!
//! The reason no scenario has ever run that production pusher is narrower than
//! it looks. `on_dispatch_tick` calls `pull_and_admit` unconditionally, dozens
//! of times per scenario, and `pull_and_admit` ends in the push. What keeps the
//! push loop empty is that `FakeGithub::dispatch_workflow` records a dispatch
//! and never a run, so `find_run` answers `None` and the intake cycle matches
//! nothing.
//!
//! One `seed_run(nonce, Completed, Success)` plus `seed_run_artifacts` — the
//! obvious next step for anyone extending this harness toward the real pull
//! path — removes that. The correspondence this harness already seeds then
//! resolves the capture's checkout to a real commit, and the push loop reaches
//! the reactor's pusher with it.
//!
//! What that pusher now is has changed (#4842): the boot seam selects on build
//! shape, so any `testing`-featured binary — every binary `cargo test` forks —
//! carries the refusing arm and cannot shell `git push` whatever backend it
//! names. A scenario extended this way therefore gets a logged refusal rather
//! than a force-push to the developer's own `origin`.
//!
//! That is a backstop, not the design. A refusal is still a scenario failing
//! for a reason that has nothing to do with what it set out to prove, so give
//! the reactor a recording pusher through `ExecutorReactorState::with_pusher`
//! before writing that scenario, and read the pushes it recorded.
//!
//! # Why it boots in-process rather than forking
//!
//! Boot construction is what decides which stores a reactor opens, and the two
//! roots it opens are named by *different* configs: the executor reactor opens
//! its journal and its artifacts handle from [`CoordinatorConfig`], while the
//! store and artifacts capabilities open theirs from [`StoreConfig`] and
//! [`ArtifactsConfig`]. Pointing each pair at one temporary root is what makes a
//! reactor that resolved a different one — a platform data dir, say — fail here
//! instead of filing real records where nothing reads them (#4705). Owning the
//! roots is also what lets a scenario read them directly, which a forked
//! coordinator's wire surface does not expose.
//!
//! # One scenario per test binary
//!
//! `GithubConnectionConfig::shared_fixture` is a process-global `OnceLock`:
//! first caller wins and it never resets. Every consumer inside one coordinator
//! wants exactly that — the correspondence store, the source shell, the
//! projection shell and the executor shell all have to see one repository — but
//! two scenarios in one process would share a repository and a mainline. So each
//! behavior gets its own binary, and this module is compiled into each.
//!
//! Each of them declares it `pub mod fixture;`, which is load-bearing rather
//! than decorative: the harness surface is one thing, and the scenarios that
//! consume it are three. A `study_index_row` reachable only from the scenario
//! that measures an attempt cost is unreachable in the other two binaries, and
//! a private module would have each of them report it as dead — a signal about
//! how this tier is split, not about the code. Declaring the module public makes
//! its surface reachable in every binary, which is what it is: the item is part
//! of the harness whether or not the binary compiling it happens to call it.
//!
//! # The cadence is off, not slow
//!
//! Every reactor's timer runs at `poll_interval_secs.max(1)`, so there is no
//! value that means "never" — `0` polls fastest. [`QUIET_POLL_SECS`] is a day,
//! which inside a scenario is the same thing as never, and progress comes from
//! the explicit ticks below. Each reactor's `wire` still fires one boot tick;
//! at boot there is nothing enqueued for it to find.
//!
//! # The shape of a scenario
//!
//! ```ignore
//! let mut harness = FixtureHarness::start("my-scenario");
//! let bloom = harness.seal_member("wp", digest(0x51));
//!
//! let construct = harness.await_order();
//! let candidate = harness.seed_capture(bloom, "wp", digest(0xC1), digest(0xC2));
//! harness.upload_admitted(&captured(&construct, candidate));
//!
//! let verify = harness.await_order();
//! harness.upload_admitted(&passed(&verify));
//! harness.land_the_fold(bloom);
//! ```

use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_bloomery::{
    Admit, AdmitResult, BloomDraft, BloomId, BloomSpec, BloomStatus, BloomView, CandidateRef, ConfigRegistry,
    Correspondence, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, Nonce, Outcome, Query,
    QueryResult, VerifyFailureSet, ViewDocument, WorkpieceId,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitDataApi, PullRequestApi, candidate_ref_name, landing_branch, to_hex};
use aether_chassis_bloomery::ControlCore;
use aether_chassis_bloomery::artifacts::{ArtifactsCapabilityState, ArtifactsConfig, GetResult};
use aether_chassis_bloomery::bloomery::{
    BloomeryChassis, BloomeryEnv, Chassis, CoordinatorConfig, DispatchTick, ExecutorReactorCapability,
    GithubConnectionConfig, IntegrateReactorCapability, IntegrateTick, LandReactorCapability, LandTick,
    ScriptedEvidence, ScriptedEvidenceResult, ScriptedUpload, ScriptedVerdict,
};
use aether_chassis_bloomery::control::ObserveTick;
use aether_chassis_bloomery::session::SessionConfig;
use aether_chassis_bloomery::signing::SigningConfig;
use aether_chassis_bloomery::store::{OutstandingOrder, SqliteStore, StoreBackend, StoreConfig};
use aether_codec::frame::{read_frame, write_frame};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_rpc::{RpcServerHandle, WireFrame};
use aether_substrate::chassis::builder::BuiltChassis;
use serde::Serialize;
use tempfile::TempDir;

use crate::common::client::{call, call_frame, connect_and_handshake};

/// A poll cadence far enough out that no reactor's timer fires inside a
/// scenario. See the module note — there is no "never", so a day stands in for
/// one and every step below is an explicit tick.
const QUIET_POLL_SECS: u64 = 86_400;

/// How long the source cap's boot reconcile may take to bind mainline to a real
/// commit. One in-process round trip, so this is a fault budget rather than a
/// settling time.
const BOOT_BUDGET: Duration = Duration::from_secs(20);

/// How long one scenario step may wait for the effect of a fact a reactor
/// admitted detached. Generous against a scheduler under a loaded CI runner, and
/// never reached by a coordinator that is making progress.
const STEP_BUDGET: Duration = Duration::from_secs(20);

/// Between re-wakes inside a waiting step.
const POLL: Duration = Duration::from_millis(20);

/// This harness's own socket read timeout, set well clear of the two budgets
/// above.
///
/// The shared client sets twenty seconds, which is exactly [`STEP_BUDGET`]. At
/// equal values a tick slow enough to matter races the budget it is being
/// measured against, and the socket usually wins — so a loaded runner reports an
/// io timeout from inside `tick` rather than the budget message that names what
/// the scenario was waiting for. Widening the socket here leaves the budget as
/// the thing that fires first, and keeps the socket error meaning what it says:
/// the coordinator stopped answering.
const SOCKET_READ_TIMEOUT: Duration = Duration::from_mins(2);

/// The commit a person merges a landing proposal at. A squash, so deliberately
/// not the head Bloomery proposed — the distinction the land watch has to get
/// right, and one a proposal-head echo would assert away.
const SQUASH_COMMIT: &str = "5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c";

/// The Git Data short form of the ref the coordinator treats as mainline. The
/// default, since no scenario repoints [`CoordinatorConfig::mainline_ref`].
const MAINLINE_REF: &str = "heads/main";

/// A live in-process scenario: a booted coordinator, the in-memory repository it
/// runs against, and the wire connection that drives and observes it.
pub struct FixtureHarness {
    /// Declared first so teardown runs before the temporary roots go: dropping
    /// the chassis stops every reactor timer and closes the store handles.
    _chassis: BuiltChassis<BloomeryChassis>,
    stream: TcpStream,
    cid: u64,
    fake: FakeGithub,
    store_path: String,
    artifacts_root: PathBuf,
    base: Digest,
    /// Holds the journal and the artifacts store. Never `keep()`-ed: the guard's
    /// `Drop` is what reclaims them on the unwind path a failed assert takes.
    _state: TempDir,
}

impl FixtureHarness {
    /// Boot a coordinator over fresh temporary stores and the process-global
    /// in-memory repository, and hold it until its mainline is sealable.
    ///
    /// # Panics
    /// The chassis did not boot, the RPC ingress did not answer, or mainline
    /// never bound to a commit inside [`BOOT_BUDGET`].
    #[must_use]
    pub fn start(client_name: &str) -> Self {
        let state = tempfile::tempdir().expect("a temporary root for the journal and the artifacts store");
        let store_path = state.path().join("bloomery.db").to_string_lossy().into_owned();
        let artifacts_root = state.path().join("artifacts").to_string_lossy().into_owned();

        let github =
            GithubConnectionConfig { github_backend: "fixture".to_owned(), ..GithubConnectionConfig::default() };
        // Both halves of each pair name the same path on purpose. The executor
        // reactor opens its own `SqliteStore` and its own artifacts handle from
        // the coordinator config, while the store and artifacts caps open theirs
        // from their own configs — so a scenario reads what the reactor wrote
        // only if the two agree (#4705).
        let coordinator = CoordinatorConfig {
            store_path: store_path.clone(),
            artifacts_root: Some(artifacts_root.clone()),
            // Every lane through the one backend a scenario can script. With the
            // local lane on, `construct.*` would route to a real subprocess.
            local_lane_enabled: false,
            poll_interval_secs: QUIET_POLL_SECS,
            ..CoordinatorConfig::default()
        };
        let env = BloomeryEnv {
            // Port 0 → OS-assigned, so concurrently running scenario binaries
            // never collide on a fixed one.
            rpc_port: 0,
            http_port: 0,
            store: StoreConfig { path: store_path.clone() },
            artifacts: ArtifactsConfig { root: Some(artifacts_root.clone()) },
            github: github.clone(),
            coordinator,
            session: SessionConfig::default(),
            signing: SigningConfig::default(),
        };

        // The same instance every shell inside the chassis got: in fixture mode
        // the fake *is* the correspondence store, so seeding through this handle
        // is how a scenario makes a digest resolvable to the coordinator.
        let fake = github.shared_fixture();
        let chassis = BloomeryChassis::build(env).expect("the fixture coordinator boots");
        let port = chassis.handle::<RpcServerHandle>().expect("the RPC ingress published its port").local_port;
        let stream = connect_and_handshake(port, client_name);
        stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT)).expect("the fixture socket takes a read timeout");

        let mut harness = Self {
            _chassis: chassis,
            stream,
            cid: 1,
            fake,
            store_path,
            artifacts_root: PathBuf::from(artifacts_root),
            base: Digest::default(),
            _state: state,
        };
        harness.base = harness.sealable_base();
        harness
    }

    /// The coordinator's mainline, checked to be a base a dispatch can actually
    /// check out.
    ///
    /// Read from the projection rather than assumed. The source cap's boot
    /// reconcile binds the repository's live head to the coordinator's mainline
    /// digest, so a scenario that hardcoded either the genesis value or the
    /// fixture's own base digest would be asserting the reconcile's outcome
    /// instead of sealing on it — and only one of the two resolves to a git
    /// object once the reconcile has run.
    ///
    /// The resolution check is the part worth keeping: a base with no recorded
    /// object refuses the very first dispatch with an unresolved checkout, which
    /// reads as a coordinator defect rather than as setup that never happened.
    fn sealable_base(&mut self) -> Digest {
        let deadline = Instant::now() + BOOT_BUDGET;
        loop {
            let mainline = self.view().mainline;
            if self.fake.resolve_backend_object(&mainline).expect("the fixture correspondence reads").is_some() {
                return mainline;
            }
            assert!(Instant::now() < deadline, "the coordinator's mainline never bound to a checkoutable commit");
            thread::sleep(POLL);
        }
    }

    /// The whole projection, right now.
    ///
    /// # Panics
    /// The query was refused or its reply did not decode.
    pub fn view(&mut self) -> ViewDocument {
        self.cid += 1;
        // `release` reads an orphan-claim release request and takes precedence over
        // `bloom` when both are set (ADR-0179); both unset is the whole-document read.
        let query = Query { bloom: None, release: None };
        match call::<_, QueryResult>(&mut self.stream, self.cid, control_mailbox(), &query) {
            QueryResult::Document { document } => from_bytes(&document).expect("the projection decodes"),
            other => panic!("expected a document reply, got {other:?}"),
        }
    }

    /// One bloom's view.
    ///
    /// # Panics
    /// The projection holds no such bloom.
    pub fn bloom(&mut self, bloom: BloomId) -> BloomView {
        self.view()
            .blooms
            .into_iter()
            .find(|view| view.id == bloom)
            .unwrap_or_else(|| panic!("the projection holds no bloom {bloom:?}"))
    }

    /// Admit one reducer fact through the control core's wire ingress and decode
    /// its outcome — the same `Admit` path every external caller uses.
    ///
    /// # Panics
    /// The control core refused the admit, or its outcome did not decode.
    pub fn admit(&mut self, key: &str, fact: Fact) -> Outcome {
        let event = Event { idempotency_key: IdempotencyKey(key.to_owned()), fact };

        self.cid += 1;
        let admit = Admit { event: to_vec(&event).expect("a reducer event encodes") };
        match call::<_, AdmitResult>(&mut self.stream, self.cid, control_mailbox(), &admit) {
            AdmitResult::Ok { outcome } => from_bytes::<Outcome>(&outcome).expect("the outcome decodes"),
            AdmitResult::Err { error } => panic!("the admit was refused: {error}"),
        }
    }

    /// Seal a single-member bloom on the observed mainline and return its id.
    ///
    /// # Panics
    /// The seal was refused.
    pub fn seal_member(&mut self, workpiece: &str, scope_revision: Digest) -> BloomId {
        let spec = draft(self.base, &[member(workpiece, scope_revision)]);
        let bloom = spec.id();
        match self.admit(&format!("fixture-seal-{workpiece}"), Fact::Seal(spec)) {
            Outcome::Sealed(sealed) => assert_eq!(sealed, bloom, "the sealed id is the spec's content address"),
            other => panic!("the fixture seal must seal: {other:?}"),
        }
        bloom
    }

    /// Wake the executor reactor until the coordinator holds exactly one
    /// outstanding order, and return it.
    ///
    /// Waking repeatedly rather than once is what absorbs the one asynchrony in
    /// the chain. A reactor that admits a fact to the control core sends it
    /// **detached** — a fresh causal chain — so the tick's own settlement says
    /// nothing about when the reducer has acted on it, and the dispatch decision
    /// that follows may not be in the outbox when the next tick fires. A drain
    /// is idempotent (an acked row does not re-drain), so re-waking costs
    /// nothing and the loop stops the moment the order exists.
    ///
    /// This waits for something the coordinator *owes*; it does not paper over
    /// a coordinator that owes nothing. A line that stopped exhausts the budget
    /// and reports it.
    ///
    /// # Panics
    /// No single order appeared inside [`STEP_BUDGET`], or two did — a scenario
    /// that dispatched nothing and one that dispatched twice are both the line
    /// moving somewhere it should not have.
    pub fn await_order(&mut self) -> OutstandingOrder {
        let deadline = Instant::now() + STEP_BUDGET;
        loop {
            self.dispatch_tick();
            let mut orders = self.orders();
            if orders.len() == 1 {
                return orders.remove(0);
            }
            assert!(orders.is_empty(), "expected one outstanding order, got {:?}", nonces(&orders));
            assert!(Instant::now() < deadline, "the coordinator dispatched nothing inside {STEP_BUDGET:?}");
            thread::sleep(POLL);
        }
    }

    /// Wake the land reactor until `bloom` reaches `want`.
    ///
    /// The same asynchrony [`await_order`](Self::await_order) absorbs, on the
    /// land side: the reactor admits its observed landing detached, so the
    /// status a scenario is waiting for lands in the projection some time after
    /// the tick that produced it settles.
    ///
    /// # Panics
    /// The bloom did not reach `want` inside [`STEP_BUDGET`].
    pub fn await_landing(&mut self, bloom: BloomId, want: BloomStatus) {
        let deadline = Instant::now() + STEP_BUDGET;
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

    /// Wake the executor reactor once: drain the dispatch topics, submit each
    /// entry, and record its outstanding order.
    pub fn dispatch_tick(&mut self) {
        self.tick(<ExecutorReactorCapability as Addressable>::resolve(0, ()), &DispatchTick::default());
    }

    /// Wake the integrate reactor once: fold the bloom's claimed candidates.
    ///
    /// One wake suffices: the scripted verdict that completed the claim set
    /// settled through the control core, so the fold's decision is already in
    /// the outbox when this fires.
    pub fn integrate_tick(&mut self) {
        self.tick(<IntegrateReactorCapability as Addressable>::resolve(0, ()), &IntegrateTick::default());
    }

    /// Wake the land reactor once: propose the resolved head, or poll the
    /// proposal already open.
    pub fn land_tick(&mut self) {
        self.tick(<LandReactorCapability as Addressable>::resolve(0, ()), &LandTick::default());
    }

    /// Wake the control core's mainline observer once: read the repository's
    /// live head and admit what it says. The same wake its own poll timer fires,
    /// which a scenario's day-long cadence never reaches.
    pub fn observe_tick(&mut self) {
        self.tick(control_mailbox(), &ObserveTick::default());
    }

    /// Move the repository's mainline to `head` — a person merging something
    /// this coordinator did not land.
    ///
    /// The commit is made resolvable as well as pointed at, so an observation
    /// reverse-resolves the ref to `head` instead of minting a digest of its own
    /// for an object nothing has named.
    pub fn move_mainline(&self, head: Digest) {
        self.fake.seed_git_object(&head);
        self.fake.seed_ref_at(MAINLINE_REF, &head);
    }

    /// Wake the observer until the coordinator's mainline reads `want`.
    ///
    /// The same asynchrony [`await_order`](Self::await_order) absorbs: the
    /// observation is admitted detached, so the projection catches up some time
    /// after the tick that produced it settles.
    ///
    /// # Panics
    /// Mainline did not reach `want` inside [`STEP_BUDGET`].
    pub fn await_mainline(&mut self, want: Digest) {
        let deadline = Instant::now() + STEP_BUDGET;
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

    /// Dispatch one reactor's tick and wait for its causal chain to settle.
    ///
    /// The wait is what makes a scenario a sequence of steps rather than a race:
    /// the reactor's whole drain — the port calls, the store writes, the orders
    /// it records — has happened by the time this returns. A tick carries no
    /// reply, so this drains to `ReplyEnd` rather than decoding one.
    fn tick<K: Kind + Serialize>(&mut self, mailbox: MailboxId, wake: &K) {
        self.cid += 1;
        write_frame(&mut self.stream, &call_frame(self.cid, mailbox, wake))
            .expect("the tick reaches the coordinator's RPC ingress");
        loop {
            match read_frame(&mut self.stream).expect("the coordinator answers the tick") {
                WireFrame::ReplyEvent { cid, .. } => assert_eq!(cid, self.cid, "ReplyEvent cid mismatch"),
                WireFrame::ReplyEnd { cid, result } => {
                    assert_eq!(cid, self.cid, "ReplyEnd cid mismatch");
                    result.expect("the tick's causal chain settled without a fault");
                    return;
                }
                other => panic!("unexpected frame for tick {}: {other:?}", self.cid),
            }
        }
    }

    /// Carry a bloom whose claim set is complete through the fold, whichever
    /// aggregate gates it dispatches, and the landing — the tail every scenario
    /// shares once its member line has done whatever that scenario is about.
    ///
    /// Returns whether the fold's *mechanical* gate actually ran. A fold that
    /// reproduces a tree the bloom already proved passes it by identity (#4891),
    /// which is the ordinary case for a single member: its fold is the candidate
    /// it verified. A scenario that cares which happened asserts on the return;
    /// the rest read it as "the gates the fold needed, passed".
    ///
    /// Every gate that does run is scripted passing, because a scenario about
    /// the member line has nothing to say about them. What is asserted here is
    /// the route each verdict took: the idempotency key names the fact the
    /// broker chose, so a gate whose verdict was routed to the wrong one fails
    /// here rather than silently resolving the bloom under a fact nobody meant.
    ///
    /// # Panics
    /// A gate did not dispatch, a verdict was refused, or the bloom did not
    /// land.
    pub fn land_the_fold(&mut self, bloom: BloomId) -> bool {
        self.integrate_tick();

        let order = self.await_order();
        assert!(order.workpiece.is_empty(), "a bloom-level order carries no member axis");
        let mut key = self.upload_admitted(&passed(&order));

        // The mechanical gate, when the fold was a tree nobody had built before.
        // Its passing verdict is what dispatches the critic; a fold that passed
        // by identity dispatched the critic already, so that first order was the
        // critic's own.
        let mechanical_ran = key.starts_with("aether.bloomery.aggregate_verify:");
        if mechanical_ran {
            let aggregate_review = self.await_order();
            key = self.upload_admitted(&passed(&aggregate_review));
        }
        assert!(key.starts_with("aether.bloomery.aggregate_review:"), "the critic's gate: {key}");

        // Landing is two steps, because mainline is protected: the reactor
        // proposes, a person accepts, and the next poll observes the acceptance.
        self.land_tick();
        let proposal = self.landing_proposal(bloom).expect("a resolved bloom proposes a landing");
        assert_eq!(self.bloom(bloom).status, BloomStatus::Resolved, "a proposed bloom has not landed yet");

        self.fake.merge_pull_request(proposal, SQUASH_COMMIT);
        self.await_landing(bloom, BloomStatus::Landed);
        mechanical_ran
    }

    /// Every order the coordinator currently holds outstanding, read from its
    /// own journal over a second connection (the store is in WAL mode, so a
    /// reader does not contend with its writer).
    ///
    /// # Panics
    /// The store could not be opened or read.
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
    /// The call was refused or its reply did not decode.
    pub fn upload(&mut self, upload: &ScriptedUpload) -> ScriptedEvidenceResult {
        self.cid += 1;
        let evidence = ScriptedEvidence { upload: to_vec(upload).expect("a scripted upload encodes") };
        call(&mut self.stream, self.cid, <ExecutorReactorCapability as Addressable>::resolve(0, ()), &evidence)
    }

    /// Upload a scripted verdict and assert the broker admitted it, returning
    /// the idempotency key the admitted event carries — which names the route
    /// the broker chose.
    ///
    /// # Panics
    /// The broker refused, or the scripted lane itself faulted.
    pub fn upload_admitted(&mut self, upload: &ScriptedUpload) -> String {
        match self.upload(upload) {
            ScriptedEvidenceResult::Admitted { idempotency_key } => idempotency_key,
            other => panic!("the scripted verdict was not admitted: {other:?}"),
        }
    }

    /// Stage the capture a construct lane would have produced: a real commit in
    /// the fixture repository carrying `tree`, published at the member's
    /// candidate ref, with `checkout` recorded as that commit's digest.
    ///
    /// Both correspondences are load-bearing. `checkout` becomes the git commit
    /// every later stage of this member checks out, so without its recording the
    /// next dispatch refuses with an unresolved checkout; `tree` is what a
    /// single-member fold states onto the integration branch. They are separate
    /// arguments because they are separate axes — the tree binds the evidence,
    /// the checkout names what the work runs on — and a scenario that conflated
    /// them would still pass while proving nothing about the pairing.
    ///
    /// The ref is addressed through [`candidate_ref_name`] so a scenario names
    /// it exactly as the fold does.
    ///
    /// # Panics
    /// The fixture repository refused to mint the capture commit.
    #[must_use]
    pub fn seed_capture(&self, bloom: BloomId, workpiece: &str, tree: Digest, checkout: Digest) -> CandidateRef {
        let tree_sha = to_hex(&tree);
        let commit = self
            .fake
            .create_commit(&format!("capture {workpiece}"), &tree_sha, &[])
            .expect("the fixture mints the capture commit");

        self.fake.seed_ref(candidate_ref_name(&bloom, workpiece).trim_start_matches("refs/"), &commit.sha);
        self.fake.seed_correspondence(&tree, &tree_sha);
        self.fake.seed_correspondence(&checkout, &commit.sha);
        CandidateRef { tree, checkout }
    }

    /// The study artifact the executor reactor filed for `(bloom, attempt)`, as
    /// its content-store digest — the index projection over the artifact bytes.
    ///
    /// # Panics
    /// The store could not be opened or read.
    #[must_use]
    pub fn study_index_row(&self, bloom: BloomId, attempt: Digest) -> Option<String> {
        SqliteStore::open(&self.store_path)
            .expect("the coordinator's journal opens for reading")
            .lookup_study(bloom.0.as_bytes(), attempt.as_bytes())
            .expect("the study index reads")
    }

    /// Fetch one artifact from the store root the chassis was configured with —
    /// a second handle on the same eviction-free content store the
    /// `aether.artifacts` capability mounts.
    ///
    /// Read through a freshly opened handle rather than through the cap's
    /// `aether.artifacts.get` mail, and the difference is not cosmetic: a
    /// [`ContentStore`](aether_substrate::content_store::ContentStore) indexes
    /// its entries in memory when it opens, so the long-lived handle the cap
    /// booted with cannot see an entry a *different* live handle wrote
    /// afterwards. Asking it would answer `NotFound` for a record that is
    /// correctly filed — a false failure, not a tripwire. Opening here re-reads
    /// the root from disk, which is exactly the question worth asking: is the
    /// study record at the root this chassis was configured with?
    ///
    /// # Panics
    /// The configured root could not be opened.
    #[must_use]
    pub fn artifact(&self, digest: &str) -> GetResult {
        ArtifactsCapabilityState::open(&self.artifacts_root)
            .expect("the configured artifacts root opens")
            .get(digest.to_owned())
    }

    /// The number of the landing proposal open for `bloom`, if the land reactor
    /// has opened one.
    ///
    /// # Panics
    /// The fixture's pull-request surface faulted.
    #[must_use]
    pub fn landing_proposal(&self, bloom: BloomId) -> Option<u64> {
        self.fake
            .find_pull_request_for_head(&landing_branch(&bloom))
            .expect("the fixture pull-request surface answers")
            .map(|pull| pull.number)
    }
}

/// The native control core's mailbox, resolved through its own addressing
/// identity — the same way the reactors resolve it.
fn control_mailbox() -> MailboxId {
    <ControlCore as Addressable>::resolve(0, ())
}

/// The nonces of a set of orders — what an unexpected-order-count failure
/// reports, since the nonce names the outbox sequence that produced it.
fn nonces(orders: &[OutstandingOrder]) -> Vec<&str> {
    orders.iter().map(|order| order.nonce.as_str()).collect()
}

/// A digest whose every byte is `seed` — the scenario shorthand for a distinct,
/// recognizable value.
#[must_use]
pub fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

/// One member, approved. The approval has to bind the member's own subject or
/// the seal door refuses it as an unapproved member (ADR-0149), and the subject
/// is a function of the rest of the member — its workpiece, its scope revision,
/// and the configs ADR-0174 folded in — so it is set after the member is
/// otherwise built.
#[must_use]
pub fn member(workpiece: &str, scope_revision: Digest) -> Membership {
    let mut member = Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail: digest(200) },
    };
    member.approval.subject = member.subject();
    member
}

/// Freeze `members` into a spec sealing on `base`.
#[must_use]
pub fn draft(base: Digest, members: &[Membership]) -> BloomSpec {
    BloomDraft { proposals: members.to_vec(), base, ..BloomDraft::default() }.seal()
}

/// The verdict a lane would have uploaded for `order`: no candidate, no
/// findings, no failed verifiers and no measured cost — the plain shape every
/// other constructor here specializes.
///
/// Bound to the order's own displayed digest, because that is the only binding
/// the broker admits: an upload naming anything else is refused before the
/// reducer sees it.
///
/// # Panics
/// The order's displayed-digest column is not 32 bytes, which only a corrupt
/// row can be.
#[must_use]
pub fn verdict(order: &OutstandingOrder, verdict: ScriptedVerdict) -> ScriptedUpload {
    let subject = Digest::from_slice(&order.displayed_digest).expect("a recorded order displays a whole digest");
    ScriptedUpload {
        nonce: Nonce(order.nonce.clone()),
        subject,
        verdict,
        detail: digest(0xDE),
        candidate: None,
        findings: None,
        failed_verifiers: VerifyFailureSet::EMPTY,
        cost: None,
    }
}

/// A passing verdict, capturing nothing — what a mechanical gate uploads, and
/// what a model lane whose stage produces no new tree uploads.
///
/// [`ScriptedVerdict::VerificationPassed`] rather than `Approved` because that
/// is the verdict the real local backend reports for *every* passing lane it
/// runs, model lanes included; the reducer's completion gate reads the two the
/// same way, so scripting the one a lane actually produces costs nothing and
/// keeps the vocabulary honest.
#[must_use]
pub fn passed(order: &OutstandingOrder) -> ScriptedUpload {
    verdict(order, ScriptedVerdict::VerificationPassed)
}

/// A passing verdict that captured `candidate` — what a construct or refine run
/// uploads once it has a tree to stand behind.
#[must_use]
pub fn captured(order: &OutstandingOrder, candidate: CandidateRef) -> ScriptedUpload {
    ScriptedUpload { candidate: Some(candidate), ..passed(order) }
}
