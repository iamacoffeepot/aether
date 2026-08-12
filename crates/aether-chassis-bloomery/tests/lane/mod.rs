//! The lane-boundary harness (#4727): a real coordinator, driven through a real
//! `git worktree add` and a real lane subprocess, with the mock lane binary as
//! the only substitution.
//!
//! # What stays real
//!
//! Everything. The forked `bloomery` bin is the production binary, booting the
//! production chassis: the `SQLite` journal, the reducer, the projection, every
//! reactor, the outbox drain, the poll timers. The dispatch runs the real
//! `ProcessTransformRunner` — the real `git worktree add --force --detach`, the
//! real environment scrub, a real child process, its real exit status, a real
//! `evidence.json` on disk, and the real candidate capture that commits the
//! scratch worktree. Only the program at the end of the argv is substituted,
//! through `AETHER_BLOOMERY_LANE_PROGRAM`.
//!
//! That boundary is the point. The existing seam substitutes a `TransformRunner`
//! and so skips every step above; four of the six failures that stopped a live
//! run live below it.
//!
//! # Why it forks rather than boots in-process
//!
//! Two reasons, and the second is the decisive one. Boot construction is what
//! decides which store a reactor opens and which backend it mounts, and a
//! scenario that builds those itself is not testing the thing that has broken.
//! And the git the dispatch shells has no `-C`: it resolves against the
//! coordinator's *process* working directory. A forked coordinator gets its own,
//! pointed at a scratch repository — so scenarios stay isolated and still run
//! concurrently, where an in-process harness would have to serialize on a
//! process-global `chdir`.
//!
//! # The shape of a scenario
//!
//! ```ignore
//! let mut harness = LaneHarness::start(LaneScript::all_passing());
//! let bloom = harness.settle("the member resolves", |bloom| bloom.members[0].resolution.is_some());
//! ```
//!
//! [`LaneHarness::settle`] polls the projection to a budget and checks both
//! liveness invariants on every poll, so a scenario never has to ask for them
//! and cannot forget to.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot set up its coordinator reports it by panicking")]
#![allow(
    clippy::disallowed_methods,
    reason = "cross-process fixtures address root caps by their rendered runtime name — the RPC Call surface under test"
)]

pub mod liveness;
pub mod repo;

use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use aether_bloomery::{
    Admit, AdmitResult, BackendObjectId, BloomDraft, BloomView, CONTROL_CORE_NAMESPACE, ConfigKind, ConfigRegistry,
    Correspondence, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, Outcome, Query,
    QueryResult, Snapshot, StageCatalog, ViewDocument, WorkpieceId,
};
use aether_chassis_bloomery::bloomery::mock_lane::{LaneRun, LaneScript, read_ledger};
use aether_chassis_bloomery::store::{SqliteCorrespondence, SqliteStore, StoreBackend};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use tempfile::TempDir;

use crate::common::client::{call, connect_and_handshake};
use crate::common::{Coordinator, free_port};
use repo::ScratchRepo;

/// How long a scenario waits for the coordinator to reach a state before giving
/// up. Generous against the coordinator's one-second poll cadence and a lane
/// that forks a process per dispatch.
const SETTLE_BUDGET: Duration = Duration::from_mins(1);

/// How long the world must hold still before a settle loop calls it quiescent
/// and judges it. Comfortably more than the poll cadence plus one dispatch, so
/// a coordinator that is merely between steps is never mistaken for one that has
/// stopped.
const QUIESCENCE: Duration = Duration::from_secs(12);

/// Between polls of the projection.
const POLL: Duration = Duration::from_millis(250);

/// A live lane-boundary scenario: a scratch repository, a forked coordinator
/// running in it, and the wire connection that drives and observes it.
pub struct LaneHarness {
    repo: ScratchRepo,
    /// Holds the run directories, the mock lane's script, and its ledger.
    runs: TempDir,
    /// Holds the journal and the artifacts store.
    state: TempDir,
    store_path: String,
    /// Killed and reaped when the harness drops, on the unwind path too.
    _coordinator: Coordinator,
    stream: TcpStream,
    cid: u64,
    base: Digest,
}

impl LaneHarness {
    /// Boot a coordinator over a fresh scratch repository, seal a
    /// single-member bloom against it, and hand back the live harness.
    ///
    /// # Panics
    /// Any setup step failed, or the seal was refused.
    pub fn start(script: &LaneScript) -> Self {
        Self::start_with(script, "wp")
    }

    /// [`start`](Self::start), naming the workpiece the sealed member covers.
    ///
    /// # Panics
    /// As [`start`](Self::start).
    pub fn start_with(script: &LaneScript, workpiece: &str) -> Self {
        Self::start_sealing(script, workpiece, None)
    }

    /// [`start`](Self::start) over a bloom that seals a stage catalog binding
    /// every stage's execution limit at `wall_clock_secs` (ADR-0177).
    ///
    /// The knob is sealed rather than ambient on purpose, and the harness has to
    /// go the same way the operator does: two blooms sealing the same catalog
    /// must terminate identically, so there is no coordinator-side override for
    /// a scenario to reach for. Seconds, so a scenario about a lane that never
    /// exits does not have to wait out the compiled line's one-hour calibration.
    ///
    /// # Panics
    /// As [`start`](Self::start).
    pub fn start_with_wall_clock(script: &LaneScript, wall_clock_secs: u64) -> Self {
        Self::start_sealing(script, "wp", Some(wall_clock_secs))
    }

    fn start_sealing(script: &LaneScript, workpiece: &str, wall_clock_secs: Option<u64>) -> Self {
        let repo = ScratchRepo::create();
        let runs = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        script.write_to(runs.path()).unwrap();

        let store_path = state.path().join("bloomery.db").to_string_lossy().into_owned();
        // The sealed base is a bloom-domain digest; the dispatch resolves it to a
        // real git object through the coordinator's own correspondence table,
        // which lives in this same database. Seeding it here is what makes the
        // scratch repository's commit the tree every lane checks out — without
        // it the very first submit refuses with an unresolved checkout.
        let base = Snapshot::GENESIS_MAINLINE;
        SqliteCorrespondence::open(&store_path).unwrap().record(&base, &backend_object(repo.head())).unwrap();
        // An authored catalog is content in the store plus an address in the
        // sealed registry — the same two halves `POST /configs` and a draft
        // patch produce, written directly here because this tier drives the
        // control core over the wire rather than through the REST api.
        let configs = wall_clock_secs.map_or_else(ConfigRegistry::default, |secs| author_catalog(&store_path, secs));

        let rpc_port = free_port();
        let coordinator = Coordinator::spawn_in(
            rpc_port,
            Some(&repo.work_dir()),
            &[
                ("AETHER_STORE_PATH", &store_path),
                ("AETHER_ARTIFACTS_ROOT", &state.path().join("artifacts").to_string_lossy()),
                // The whole point: the dispatch below the spawn stays real, the
                // program at the end of the argv does not.
                ("AETHER_BLOOMERY_LANE_PROGRAM", env!("CARGO_BIN_EXE_bloomery-mock-lane")),
                ("AETHER_GITHUB_LOCAL_WORKTREE_BASE", &runs.path().to_string_lossy()),
                // Every lane local, including the mechanical verify one. The
                // production default routes verify at a shared runner, which
                // needs a GitHub connection this tier deliberately does not
                // configure — and the verify lane is where half the catalogue's
                // failure modes live.
                ("AETHER_GITHUB_LOCAL_LANE_COMMANDS", "construct.,review.,verify."),
                ("AETHER_GITHUB_POLL_INTERVAL_SECS", "1"),
                // In-memory GitHub for the aggregate line (#4732): the member
                // line alone needs no GitHub, but Integrate→AggregateVerify→
                // AggregateReview→Land do. `fake` mounts every reactor with an
                // in-memory double and needs no token/owner/repo.
                ("AETHER_GITHUB_BACKEND", "fixture"),
                ("AETHER_GITHUB_FIXTURE_BASE_SHA", repo.head()),
                // A fixed capture identity, so a candidate commit never depends
                // on whatever git identity the host running the suite has.
                ("AETHER_BLOOMERY_OPERATOR_NAME", "lane harness"),
                ("AETHER_BLOOMERY_OPERATOR_EMAIL", "lane-harness@example.test"),
            ],
        );

        let stream = connect_and_handshake(rpc_port, "lane-boundary-harness");

        let mut harness = Self { repo, runs, state, store_path, _coordinator: coordinator, stream, cid: 1, base };
        harness.seal(workpiece, configs);
        harness
    }

    /// The scratch repository the coordinator runs in.
    pub const fn repo(&self) -> &ScratchRepo {
        &self.repo
    }

    /// Where the run directories, script, and ledger live.
    pub fn runs_dir(&self) -> PathBuf {
        self.runs.path().to_owned()
    }

    /// Every lane run the mock has recorded, in dispatch order.
    ///
    /// # Panics
    /// The ledger exists but could not be read.
    pub fn ledger(&self) -> Vec<LaneRun> {
        read_ledger(self.runs.path()).unwrap()
    }

    /// The nonces the store still holds as outstanding orders.
    ///
    /// Read from the coordinator's own journal over a second connection; the
    /// store is in WAL mode, so a reader does not contend with its writer.
    ///
    /// # Panics
    /// The store could not be opened or read.
    pub fn outstanding(&self) -> Vec<String> {
        SqliteStore::open(&self.store_path).unwrap().list_outstanding_nonces().unwrap()
    }

    /// The whole projection, right now.
    ///
    /// # Panics
    /// The query was refused or its reply did not decode.
    pub fn view(&mut self) -> ViewDocument {
        self.cid += 1;
        match call::<_, QueryResult>(
            &mut self.stream,
            self.cid,
            control_mailbox(),
            &Query { bloom: None, release: None },
        ) {
            QueryResult::Document { document } => from_bytes(&document).expect("the projection decodes"),
            other => panic!("expected a document reply, got {other:?}"),
        }
    }

    /// Poll until `want` holds of the (single) bloom, checking both liveness
    /// invariants on every poll.
    ///
    /// `label` names what the scenario is waiting for, and is what a budget
    /// exhaustion reports.
    ///
    /// # Panics
    /// The budget expired, or the coordinator went quiescent with work still
    /// owed — the liveness invariant, which no scenario opts into and none can
    /// forget.
    pub fn settle(&mut self, label: &str, want: impl Fn(&BloomView) -> bool) -> BloomView {
        let deadline = Instant::now() + SETTLE_BUDGET;
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
            } else if still_since.elapsed() >= QUIESCENCE {
                // The world stopped moving before reaching what the scenario
                // asked for. Whether that is legitimate is not the scenario's
                // call — it is the invariant's.
                self.judge_quiescence(label, &document);
                panic!(
                    "{label}: the coordinator settled into a legitimate stop without reaching it — {:?}",
                    document.blooms.first().map(|bloom| bloom.status),
                );
            }

            assert!(
                Instant::now() < deadline,
                "{label}: not reached inside {SETTLE_BUDGET:?}; outstanding={:?} runs={}",
                self.outstanding(),
                self.ledger().len(),
            );
            thread::sleep(POLL);
        }
    }

    /// Assert that the coordinator's current standstill is one it is entitled
    /// to — a terminal state or an accountable wedge, never a stall.
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

    /// Seal a single-member bloom on the harness's base, under `configs`.
    fn seal(&mut self, workpiece: &str, configs: ConfigRegistry) {
        let mut member = Membership {
            workpiece: WorkpieceId(workpiece.to_owned()),
            scope_revision: Digest::from_bytes([1; 32]),
            configs: ConfigRegistry::default(),
            approval: Evidence {
                subject: Digest::default(),
                kind: EvidenceKind::Approval,
                detail: Digest::from_bytes([200; 32]),
            },
        };
        // The approval binds the member's own subject (ADR-0174).
        member.approval.subject = member.subject();
        let spec = BloomDraft { proposals: vec![member], base: self.base, configs, ..BloomDraft::default() }.seal();
        let event = Event { idempotency_key: IdempotencyKey("lane-seal".to_owned()), fact: Fact::Seal(spec) };

        self.cid += 1;
        let admit = Admit { event: to_vec(&event).unwrap() };
        let outcome = match call::<_, AdmitResult>(&mut self.stream, self.cid, control_mailbox(), &admit) {
            AdmitResult::Ok { outcome } => from_bytes::<Outcome>(&outcome).expect("the outcome decodes"),
            AdmitResult::Err { error } => panic!("the harness seal was refused: {error}"),
        };
        assert!(matches!(outcome, Outcome::Sealed(_)), "the harness seal must seal: {outcome:?}");
    }
}

/// The native control core's mailbox, addressed by its lineage path.
fn control_mailbox() -> MailboxId {
    mailbox_id_from_path(CONTROL_CORE_NAMESPACE)
}

/// Author a stage catalog binding every stage's execution limit at
/// `wall_clock_secs` into the store at `store_path`, and return the registry
/// that seals its address.
///
/// Written before the coordinator forks, the way the correspondence row is: the
/// control core resolves a sealed address by reading this table, and a seal
/// naming an address with no content behind it is refused rather than defaulted.
///
/// # Panics
/// The store could not be opened or written.
fn author_catalog(store_path: &str, wall_clock_secs: u64) -> ConfigRegistry {
    let mut catalog = StageCatalog::line();
    for binding in &mut catalog.bindings {
        binding.wall_clock_secs = wall_clock_secs;
    }
    let bytes = to_vec(&catalog).unwrap();
    let address = catalog.address();
    SqliteStore::open(store_path).unwrap().record_config(address.as_bytes(), StageCatalog::NAME, &bytes).unwrap();

    let mut configs = ConfigRegistry::default();
    configs.insert::<StageCatalog>(address);
    configs
}

/// The scratch repository's real HEAD sha as the opaque backend object the
/// correspondence stores — the same bytes the coordinator's own capture path
/// decodes out of `git rev-parse`, decoded here so the harness seeds the store
/// without naming a Git-adapter type.
///
/// # Panics
/// `sha` is not even-length lowercase-or-uppercase hex, which `git rev-parse`
/// never prints.
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
