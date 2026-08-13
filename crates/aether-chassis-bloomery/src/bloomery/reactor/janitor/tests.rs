//! The sweep core over a real `SqliteStore` and a stub runner — the journal
//! and spawn seams the running reactor drives, without the mail harness.
//!
//! The bug this reactor exists to close is a kill or crash that never took the
//! happy-path release: the worktree and evidence sit on disk, the journal
//! already knows the run is terminal, and nothing reclaims them until a
//! coordinator restart (and even then only the boot reconcile's abandoned
//! nonce-keyed checkouts). These tests pin that the sweep reads the journal
//! and the stub runner, not the child's exit status.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use aether_bloomery::{
    BloomDraft, BloomId, Digest, Event, Evidence, EvidenceKind, Fact, Forecast, IdempotencyKey, Membership, WorkpieceId,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitSource, MainlineRef, candidate_ref_name};
use aether_data::wire::to_vec;

use super::sweep::{JanitorPolicy, SweepRequest, retention_duration, sweep};
use crate::bloomery::SourceShell;
use crate::bloomery::{CapturedObjects, LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner};
use crate::store::{OutstandingOrder, SqliteStore, StoreBackend};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn membership(name: &str, revision: u8) -> Membership {
    let mut member = Membership {
        workpiece: WorkpieceId(name.into()),
        scope_revision: digest(revision),
        configs: aether_bloomery::ConfigRegistry::default(),
        approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
    };
    member.approval = Evidence { subject: member.subject(), kind: EvidenceKind::Approval, detail: digest(200) };
    member
}

fn draft(base: u8, members: Vec<Membership>) -> BloomDraft {
    BloomDraft { proposals: members, base: digest(base), ..BloomDraft::default() }
}

fn event(key: &str, fact: Fact) -> Event {
    Event { idempotency_key: IdempotencyKey(key.into()), fact }
}

fn journal(store: &mut SqliteStore, key: &str, fact: Fact) {
    store.append_event(key, &to_vec(&event(key, fact)).unwrap()).unwrap();
}

/// Seal a predecessor and supersede it, so the journal describes one terminal
/// bloom and one live successor. The predecessor is the kill/crash leak case:
/// its dispatches are done and its dirs/refs are reclaimable.
fn journal_superseded(store: &mut SqliteStore) -> (BloomId, BloomId) {
    // Base 0 is `Snapshot::GENESIS_MAINLINE`, so a successor that keeps the
    // same base is not a rebase and does not need an observed head.
    let predecessor = draft(0, vec![membership("wp", 1)]).seal();
    let mut successor = draft(0, vec![membership("wp", 1)]);
    successor.forecast = Forecast { predicted_retries: 1, ..Forecast::default() };
    let successor = successor.seal();
    journal(store, "seal", Fact::Seal(predecessor.clone()));
    journal(store, "supersede", Fact::Supersede { predecessor: predecessor.id(), successor: successor.clone() });
    (predecessor.id(), successor.id())
}

fn journal_sealed(store: &mut SqliteStore) -> BloomId {
    let spec = draft(0, vec![membership("wp", 1)]).seal();
    journal(store, "seal", Fact::Seal(spec.clone()));
    spec.id()
}

fn order_for(nonce: &str, bloom: &BloomId) -> OutstandingOrder {
    OutstandingOrder {
        profile: Vec::new(),
        nonce: nonce.to_owned(),
        bloom: bloom.0.as_bytes().to_vec(),
        workpiece: "wp".to_owned(),
        scope_revision: vec![1; 32],
        candidate: vec![5; 32],
        displayed_digest: vec![5; 32],
        stage: vec![9],
        transformation: vec![7, 7],
        configs: vec![3, 3],
        deadline_unix_millis: 1_700_000_060_000,
    }
}

struct Released(Arc<Mutex<Vec<PathBuf>>>);

struct StubRunner {
    registered: Vec<PathBuf>,
    released: Released,
}

impl TransformRunner for StubRunner {
    fn start(&self, _spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        Ok(Box::new(StubProcess))
    }

    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        self.released.0.lock().unwrap().push(worktree_dir.to_owned());
        Ok(())
    }

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError> {
        Ok(self.registered.clone())
    }

    fn capture(
        &self,
        _worktree_dir: &Path,
        _message: Option<&str>,
    ) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        Ok(None)
    }
}

struct StubProcess;

impl RunProcess for StubProcess {
    fn poll(&mut self) -> RunLifecycle {
        RunLifecycle::Running
    }

    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        Ok(())
    }
}

fn policy(retention_days: u64, budget_bytes: u64) -> JanitorPolicy {
    JanitorPolicy { lane_target_budget_bytes: budget_bytes, evidence_retention_days: retention_days }
}

fn run(request: &mut SweepRequest<'_>) -> super::sweep::SweepReport {
    sweep(request).unwrap()
}

fn age_dir(path: &Path, now: SystemTime, days: u64) {
    let dest = now.checked_sub(retention_duration(days)).expect("the fixture clock sits after the retention window");
    fs::write(path.join(".aged"), []).unwrap();
    fs::File::open(path).unwrap().set_modified(dest).unwrap();
}

#[test]
fn a_killed_runs_worktree_is_reclaimed_once_its_bloom_is_terminal() {
    // The motivating leak: the run ended by kill or crash, so the happy-path
    // release never ran, and the bloom has since been superseded. The journal
    // already knows the run is terminal; the sweep must reclaim the checkout
    // without a coordinator restart.
    let scratch = tempfile::tempdir().unwrap();
    let nonce = "killed-nonce";
    let worktree = scratch.path().join(nonce);
    fs::create_dir_all(&worktree).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    journal_superseded(&mut store);

    let released = Released(Arc::new(Mutex::new(Vec::new())));
    let runner = StubRunner { registered: vec![worktree.clone()], released: Released(Arc::clone(&released.0)) };
    let now = SystemTime::now();
    let keep = policy(7, u64::MAX);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes_running: false,
        policy: &keep,
        now,
    });

    assert_eq!(report.worktrees, 1, "the killed run's checkout is reclaimed");
    assert_eq!(released.0.lock().unwrap().as_slice(), [worktree]);
}

#[test]
fn a_lane_slots_checkout_is_never_reclaimed() {
    // Slot checkouts are the host's build paths, reused across dispatches.
    // Reclaiming one because no nonce names it would undo the cache layout
    // and pull the tree out from under whoever holds the slot.
    let scratch = tempfile::tempdir().unwrap();
    let worktree = scratch.path().join("slot-0");
    fs::create_dir_all(&worktree).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    journal_superseded(&mut store);

    let released = Released(Arc::new(Mutex::new(Vec::new())));
    let runner = StubRunner { registered: vec![worktree], released: Released(Arc::clone(&released.0)) };
    let keep = policy(7, u64::MAX);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes_running: false,
        policy: &keep,
        now: SystemTime::now(),
    });

    assert_eq!(report.worktrees, 0);
    assert!(released.0.lock().unwrap().is_empty());
}

#[test]
fn a_live_blooms_outstanding_worktree_is_spared() {
    // A dispatch still outstanding against a sealed bloom is in flight, whether
    // or not its child is reachable. Reclaiming it is the opposite of the
    // kill/crash case: it would pull the tree out from under a live lane.
    let scratch = tempfile::tempdir().unwrap();
    let nonce = "live-nonce";
    let worktree = scratch.path().join(nonce);
    fs::create_dir_all(&worktree).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    let bloom = journal_sealed(&mut store);
    store.record_order(&order_for(nonce, &bloom)).unwrap();

    let released = Released(Arc::new(Mutex::new(Vec::new())));
    let runner = StubRunner { registered: vec![worktree], released: Released(Arc::clone(&released.0)) };
    let keep = policy(7, u64::MAX);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes_running: false,
        policy: &keep,
        now: SystemTime::now(),
    });

    assert_eq!(report.worktrees, 0, "a live bloom's outstanding checkout stays");
    assert!(released.0.lock().unwrap().is_empty());
}

#[test]
fn consumed_evidence_of_a_terminal_bloom_is_kept_inside_the_retention_window() {
    // ADR-0184's calibration ledger (and forensics) read study artefacts after
    // intake. Deleting a just-consumed directory the day the bloom lands would
    // silently drop those inputs inside the configured window.
    let scratch = tempfile::tempdir().unwrap();
    let nonce = "fresh-evidence";
    let evidence = scratch.path().join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    let (predecessor, _) = journal_superseded(&mut store);
    store.record_order(&order_for(nonce, &predecessor)).unwrap();
    store.consume_order(nonce).unwrap();

    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let now = SystemTime::now();
    let keep = policy(7, u64::MAX);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes_running: false,
        policy: &keep,
        now,
    });

    assert_eq!(report.evidence_dirs, 0);
    assert!(evidence.exists(), "evidence inside the 7-day window is kept");
}

#[test]
fn consumed_evidence_of_a_terminal_bloom_is_reclaimed_after_the_retention_window() {
    let scratch = tempfile::tempdir().unwrap();
    let nonce = "stale-evidence";
    let evidence = scratch.path().join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    let (predecessor, _) = journal_superseded(&mut store);
    store.record_order(&order_for(nonce, &predecessor)).unwrap();
    store.consume_order(nonce).unwrap();

    let now = SystemTime::now();
    age_dir(&evidence, now, 8);

    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let keep = policy(7, u64::MAX);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes_running: false,
        policy: &keep,
        now,
    });

    assert_eq!(report.evidence_dirs, 1);
    assert!(!evidence.exists(), "evidence past the window of a terminal bloom is reclaimed");
}

#[test]
fn target_dirs_are_swept_when_over_budget_and_idle() {
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("slot-0-target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("cache"), vec![0_u8; 64]).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 16);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes_running: false,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 1);
    assert!(!target.exists(), "an idle host over budget drops the slot target dirs");
}

#[test]
fn target_dirs_are_not_swept_while_a_lane_is_running() {
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("slot-0-target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("cache"), vec![0_u8; 64]).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 16);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes_running: true,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 0);
    assert!(target.exists(), "a live lane's target dir is never swept, even over budget");
}

#[test]
fn terminal_bloom_working_refs_are_pruned_and_claim_refs_are_spared() {
    let scratch = tempfile::tempdir().unwrap();
    let mut store = SqliteStore::open(":memory:").unwrap();
    let (predecessor, _) = journal_superseded(&mut store);

    let fake = FakeGithub::new();
    let candidate = candidate_ref_name(&predecessor, "wp").trim_start_matches("refs/").to_owned();
    fake.seed_ref(&candidate, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let source =
        SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake.clone()), true, MainlineRef::default())));
    let integration = format!("heads/bloom/{}/integration", short_hex(&predecessor));
    let checkpoint = format!("heads/bloom/{}/checkpoint/{}", short_hex(&predecessor), "00".repeat(32));
    let landing = format!("heads/bloom/{}/landing", short_hex(&predecessor));
    fake.seed_ref(&integration, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    fake.seed_ref(&checkpoint, "cccccccccccccccccccccccccccccccccccccccc");
    fake.seed_ref(&landing, "dddddddddddddddddddddddddddddddddddddddd");
    fake.seed_ref("bloomery/admission/mainline", "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
    fake.seed_ref("bloomery/claims/wp", "ffffffffffffffffffffffffffffffffffffffff");

    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let keep = policy(7, u64::MAX);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: Some(&source),
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes_running: false,
        policy: &keep,
        now: SystemTime::now(),
    });

    assert_eq!(report.refs, 3, "candidate + integration + checkpoint of the terminal bloom");
    assert!(!fake.ref_exists(&candidate));
    assert!(!fake.ref_exists(&integration));
    assert!(!fake.ref_exists(&checkpoint));
    assert!(fake.ref_exists(&landing), "the landing branch is not this issue's to delete");
    assert!(fake.ref_exists("bloomery/admission/mainline"), "claim refs have their own reactor");
    assert!(fake.ref_exists("bloomery/claims/wp"), "a workpiece claim is untouched");
}

fn short_hex(bloom: &BloomId) -> String {
    aether_bloomery_github::short_hex(&bloom.0)
}
