#![cfg(all(unix, feature = "github"))]

//! A bloom runs start to finish against a fleet-local bare authority, with no
//! network: real `SQLite` stores, the local git-data source, the real transform
//! runner pointed at `bloomery-mock-lane`, real capture and publication, a
//! restart between resolve and land, and a mirror double that fails without
//! reversing the land (ADR-0199 slice 1).

mod common;

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_bloomery::{
    Admit, AdmitResult, BloomDraft, BloomId, BloomStatus, BloomView, ConfigRegistry, Correspondence, Digest, Event,
    Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, Outcome, ProjectedReceipt, ProjectionBackend, Query,
    QueryResult, Snapshot, Topic, ViewDocument, WorkpieceId,
};
use aether_bloomery_github::{GithubError, candidate_ref_name};
use aether_chassis_bloomery::ControlCore;
use aether_chassis_bloomery::artifacts::ArtifactsConfig;
use aether_chassis_bloomery::bloomery::mock_lane::{CANDIDATE_FILE, LaneScript};
use aether_chassis_bloomery::bloomery::{
    BloomeryChassis, BloomeryEnv, Chassis, CoordinatorConfig, DispatchTick, ExecutorReactorCapability,
    GithubConnectionConfig, IntegrateReactorCapability, IntegrateTick, LandReactorCapability, LandTick,
    ProjectionShell, TopicOutbox,
};
use aether_chassis_bloomery::session::SessionConfig;
use aether_chassis_bloomery::signing::SigningConfig;
use aether_chassis_bloomery::store::{SqliteCorrespondence, SqliteStore, StoreBackend, StoreConfig};
use aether_codec::frame::{read_frame, write_frame};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_rpc::{RpcServerHandle, WireFrame};
use aether_substrate::chassis::builder::BuiltChassis;
use serde::Serialize;
use tempfile::TempDir;

use crate::common::client::{call, call_frame, connect_and_handshake};

const WORKPIECE: &str = "wp";
const QUIET_POLL_SECS: u64 = 86_400;
const BOOT_BUDGET: Duration = Duration::from_secs(20);
const STEP_BUDGET: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);
const SOCKET_READ_TIMEOUT: Duration = Duration::from_mins(2);

#[test]
fn a_local_authority_bloom_lands_after_a_restart_and_a_failing_mirror() {
    let authority = BareAuthority::create();
    let state = tempfile::tempdir().expect("journal and artifacts root");
    let runs = tempfile::tempdir().expect("lane worktree base");
    LaneScript::all_passing().write_to(runs.path()).expect("the mock-lane script writes");

    let store_path = state.path().join("bloomery.db").to_string_lossy().into_owned();
    let artifacts_root = state.path().join("artifacts").to_string_lossy().into_owned();
    let worktree_base = runs.path().to_string_lossy().into_owned();

    let (bloom, sealed_on, first_mainline) = {
        // Land is gated off so resolve can be observed. The control core
        // nudges the land reactor the moment review passes; with CAS on, the
        // bloom is Landed before this loop can see Resolved.
        let mut harness =
            Harness::boot(&authority, &store_path, &artifacts_root, &worktree_base, "local-authority-1", false);
        let sealed_on = harness.view().mainline;
        let first_mainline = authority.rev_parse("refs/heads/main");
        let bloom = harness.seal_member(WORKPIECE);

        harness.pump_until("the bloom resolves against the local authority", |harness| {
            harness.bloom(bloom).status == BloomStatus::Resolved
        });

        assert_eq!(
            authority.rev_parse("refs/heads/main"),
            first_mainline,
            "resolve must not move mainline; land is the compare-and-swap",
        );
        assert_candidate_is_a_commit_wrapping_a_tree(&authority, bloom);
        (bloom, sealed_on, first_mainline)
    };

    let mut harness =
        Harness::boot(&authority, &store_path, &artifacts_root, &worktree_base, "local-authority-2", true);
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Resolved, "the journal replayed the already-produced head");
    harness.land_until(bloom, BloomStatus::Landed);

    assert_ne!(harness.view().mainline, sealed_on, "the receipt advanced coordinator mainline");
    assert_ne!(authority.rev_parse("refs/heads/main"), first_mainline, "the bare authority's mainline moved");
    assert_eq!(git_in(authority.path(), &["cat-file", "-t", "refs/heads/main"]).trim(), "commit");
    let landed_names = git_in(authority.path(), &["ls-tree", "-r", "--name-only", "refs/heads/main"]);
    assert!(
        landed_names.lines().any(|name| name == CANDIDATE_FILE),
        "the landed tree carries the mock lane's edit: {landed_names}"
    );

    let receipt = landing_receipt(&store_path);
    assert_eq!(receipt.receipt.bloom, bloom);
    assert_eq!(receipt.receipt.previous_base, sealed_on);
    assert_eq!(receipt.receipt.new_head, harness.view().mainline);
    assert_eq!(receipt.members, [WorkpieceId(WORKPIECE.to_owned())]);

    let landed_head = authority.rev_parse("refs/heads/main");
    let mirror = Arc::new(FailingMirror::new());
    let shell = ProjectionShell::new(Arc::clone(&mirror) as Arc<_>);
    mirror.fail();
    shell.project_receipt(&receipt).expect_err("the first replication is the failure this scenario exists to see");
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed, "a failed mirror must not un-land the bloom");
    assert_eq!(authority.rev_parse("refs/heads/main"), landed_head, "a failed mirror must not move mainline back");

    mirror.allow();
    shell.project_receipt(&receipt).expect("replication retries after the outage");
    assert_eq!(mirror.receipts(), 1, "the retry delivered the same receipt, once");
    assert_eq!(harness.bloom(bloom).status, BloomStatus::Landed);
    assert_eq!(authority.rev_parse("refs/heads/main"), landed_head);
}

/// The capture published at the member's candidate ref is a commit whose tree
/// is a tree — the #5025/#5027 pairing. Landing a tree SHA as if it were a
/// commit, or checking one out without the wrapper, is the defect class this
/// hermetic path exists to make CI-visible.
fn assert_candidate_is_a_commit_wrapping_a_tree(authority: &BareAuthority, bloom: BloomId) {
    let candidate = candidate_ref_name(&bloom, WORKPIECE);
    let checkout = authority.rev_parse(&candidate);
    assert_eq!(git_in(authority.path(), &["cat-file", "-t", &checkout]).trim(), "commit");
    let tree = git_in(authority.path(), &["rev-parse", &format!("{checkout}^{{tree}}")]);
    assert_eq!(git_in(authority.path(), &["cat-file", "-t", tree.trim()]).trim(), "tree");
}

fn landing_receipt(store_path: &str) -> ProjectedReceipt {
    let mut store = SqliteStore::open(store_path).expect("the journal reopens");
    let entries = store.drain_topic(Topic::LandingReceipt).expect("the landing-receipt topic drains");
    let entry = entries.first().expect("land emitted a receipt");
    from_bytes(&entry.payload).expect("the receipt decodes")
}

struct BareAuthority {
    _root: TempDir,
    path: PathBuf,
}

impl BareAuthority {
    fn create() -> Self {
        let root = tempfile::tempdir().expect("authority root");
        let seed = root.path().join("seed");
        fs::create_dir(&seed).expect("seed dir");
        run_git(&seed, &["init", "--quiet", "-b", "main"]);
        run_git(&seed, &["config", "--local", "user.name", "test"]);
        run_git(&seed, &["config", "--local", "user.email", "test@example.test"]);
        fs::write(seed.join("README.md"), "the sealed subject a local-authority bloom checks out.\n")
            .expect("seed file");
        run_git(&seed, &["add", "--all"]);
        run_git(&seed, &["commit", "--quiet", "--message", "subject"]);

        let path = root.path().join("authority.git");
        assert!(
            Command::new("git")
                .args(["clone", "--bare", "--quiet"])
                .arg(&seed)
                .arg(&path)
                .status()
                .expect("clone")
                .success(),
            "clone --bare into {}",
            path.display()
        );
        Self { _root: root, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn rev_parse(&self, rev: &str) -> String {
        git_in(&self.path, &["rev-parse", rev]).trim().to_owned()
    }
}

struct Harness {
    _chassis: BuiltChassis<BloomeryChassis>,
    stream: TcpStream,
    cid: u64,
    store_path: String,
}

impl Harness {
    fn boot(
        authority: &BareAuthority,
        store_path: &str,
        artifacts_root: &str,
        worktree_base: &str,
        client: &str,
        cas_land_enabled: bool,
    ) -> Self {
        let env = BloomeryEnv {
            rpc_port: 0,
            http_port: 0,
            store: StoreConfig { path: store_path.to_owned() },
            artifacts: ArtifactsConfig { root: Some(artifacts_root.to_owned()) },
            github: GithubConnectionConfig { cas_land_enabled, ..GithubConnectionConfig::default() },
            coordinator: CoordinatorConfig {
                store_path: store_path.to_owned(),
                artifacts_root: Some(artifacts_root.to_owned()),
                poll_interval_secs: QUIET_POLL_SECS,
                local_lane_enabled: true,
                local_lane_commands: "construct.,review.,verify.".to_owned(),
                local_lane_program: env!("CARGO_BIN_EXE_bloomery-mock-lane").to_owned(),
                local_worktree_base: worktree_base.to_owned(),
                authority_backend: "local".to_owned(),
                authority_repo: authority.path().to_string_lossy().into_owned(),
                operator_name: "local-authority harness".to_owned(),
                operator_email: "local-authority@example.test".to_owned(),
                ..CoordinatorConfig::default()
            },
            session: SessionConfig::default(),
            signing: SigningConfig::default(),
        };

        let chassis = BloomeryChassis::build(env).expect("the local-authority coordinator boots");
        let port = chassis.handle::<RpcServerHandle>().expect("the RPC ingress published its port").local_port;
        let stream = connect_and_handshake(port, client);
        stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT)).expect("the fixture socket takes a read timeout");

        let harness = Self { _chassis: chassis, stream, cid: 1, store_path: store_path.to_owned() };
        harness.wait_for_sealable_base();
        harness
    }

    fn wait_for_sealable_base(&self) {
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

    fn view(&mut self) -> ViewDocument {
        self.cid += 1;
        let query = Query { bloom: None, release: None, calibration: false };
        match call::<_, QueryResult>(&mut self.stream, self.cid, control_mailbox(), &query) {
            QueryResult::Document { document } => from_bytes(&document).expect("the projection decodes"),
            other => panic!("expected a document reply, got {other:?}"),
        }
    }

    fn bloom(&mut self, bloom: BloomId) -> BloomView {
        self.view()
            .blooms
            .into_iter()
            .find(|view| view.id == bloom)
            .unwrap_or_else(|| panic!("the projection holds no bloom {bloom:?}"))
    }

    fn admit(&mut self, key: &str, fact: Fact) -> Outcome {
        let event = Event { idempotency_key: IdempotencyKey(key.to_owned()), fact };
        self.cid += 1;
        let admit = Admit { event: to_vec(&event).expect("a reducer event encodes") };
        match call::<_, AdmitResult>(&mut self.stream, self.cid, control_mailbox(), &admit) {
            AdmitResult::Ok { outcome } => from_bytes::<Outcome>(&outcome).expect("the outcome decodes"),
            AdmitResult::Err { error } => panic!("the admit was refused: {error}"),
        }
    }

    fn seal_member(&mut self, workpiece: &str) -> BloomId {
        let spec = draft(self.view().mainline, &[member(workpiece, digest(0x51))]);
        let bloom = spec.id();
        match self.admit("local-authority-seal", Fact::Seal(spec)) {
            Outcome::Sealed(sealed) => assert_eq!(sealed, bloom),
            other => panic!("the local-authority seal must seal: {other:?}"),
        }
        bloom
    }

    fn pump_until(&mut self, what: &str, pred: impl Fn(&mut Self) -> bool) {
        let deadline = Instant::now() + STEP_BUDGET;
        loop {
            self.dispatch_tick();
            self.integrate_tick();
            if pred(self) {
                return;
            }
            if Instant::now() >= deadline {
                let view = self.bloom_debug();
                let orders = self.orders();
                panic!("{what} did not happen inside {STEP_BUDGET:?}; {view}; outstanding={orders:?}");
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

    fn orders(&self) -> Vec<(String, String)> {
        let mut store = SqliteStore::open(&self.store_path).expect("the journal opens");
        store
            .list_outstanding_nonces()
            .expect("outstanding nonces")
            .into_iter()
            .filter_map(|nonce| {
                store.lookup_order(&nonce).expect("order lookup").map(|order| (order.nonce, order.workpiece))
            })
            .collect()
    }

    fn land_until(&mut self, bloom: BloomId, want: BloomStatus) {
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

    fn dispatch_tick(&mut self) {
        self.tick(<ExecutorReactorCapability as Addressable>::resolve(0, ()), &DispatchTick::default());
    }

    fn integrate_tick(&mut self) {
        self.tick(<IntegrateReactorCapability as Addressable>::resolve(0, ()), &IntegrateTick::default());
    }

    fn land_tick(&mut self) {
        self.tick(<LandReactorCapability as Addressable>::resolve(0, ()), &LandTick::default());
    }

    fn tick<K: Kind + Serialize>(&mut self, mailbox: MailboxId, wake: &K) {
        self.cid += 1;
        write_frame(&mut self.stream, &call_frame(self.cid, mailbox, wake)).expect("the tick reaches the coordinator");
        loop {
            match read_frame(&mut self.stream).expect("the coordinator answers the tick") {
                WireFrame::ReplyEvent { cid, .. } => assert_eq!(cid, self.cid),
                WireFrame::ReplyEnd { cid, result } => {
                    assert_eq!(cid, self.cid);
                    result.expect("the tick's causal chain settled without a fault");
                    return;
                }
                other => panic!("unexpected frame for tick {}: {other:?}", self.cid),
            }
        }
    }
}

struct FailingMirror {
    fail: AtomicBool,
    receipts: Mutex<usize>,
}

impl FailingMirror {
    fn new() -> Self {
        Self { fail: AtomicBool::new(false), receipts: Mutex::new(0) }
    }

    fn fail(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }

    fn allow(&self) {
        self.fail.store(false, Ordering::SeqCst);
    }

    fn receipts(&self) -> usize {
        *self.receipts.lock().expect("mirror receipt count")
    }
}

impl ProjectionBackend for FailingMirror {
    type Error = GithubError;

    fn reconcile_view(&self, _view: &ViewDocument) -> Result<(), GithubError> {
        Ok(())
    }

    fn project_receipt(&self, _receipt: &ProjectedReceipt) -> Result<(), GithubError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(GithubError::Status { status: 503, body: "mirror unreachable".to_owned() });
        }
        *self.receipts.lock().expect("mirror receipt count") += 1;
        Ok(())
    }

    fn project_commission(&self, _projection: &aether_bloomery::CommissionProjection) -> Result<u64, GithubError> {
        Ok(0)
    }
}

fn control_mailbox() -> MailboxId {
    <ControlCore as Addressable>::resolve(0, ())
}

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn member(workpiece: &str, scope_revision: Digest) -> Membership {
    let mut member = Membership {
        workpiece: WorkpieceId(workpiece.to_owned()),
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail: digest(200) },
    };
    member.approval.subject = member.subject();
    member
}

fn draft(base: Digest, members: &[Membership]) -> aether_bloomery::BloomSpec {
    BloomDraft { proposals: members.to_vec(), base, ..BloomDraft::default() }.seal()
}

fn run_git(dir: &Path, args: &[&str]) {
    assert!(Command::new("git").current_dir(dir).args(args).status().expect("git status").success(), "git {args:?}");
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(dir).args(args).output().expect("git output");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}
