//! The sweep core over a real `SqliteStore` and a stub runner — the journal
//! and spawn seams the running reactor drives, without the mail harness.
//!
//! The bug this reactor exists to close is a kill or crash that never took the
//! happy-path release: the worktree and evidence sit on disk, the journal
//! already knows the run is terminal, and nothing reclaims them until a
//! coordinator restart (and even then only the boot reconcile's abandoned
//! nonce-keyed checkouts). These tests pin that the sweep reads the journal
//! and the stub runner, not the child's exit status.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aether_bloomery::testing::{draft, event, membership};
use aether_bloomery::{BloomId, Fact, Forecast};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitSource, MainlineRef, candidate_ref_name};
use aether_data::wire::to_vec;

use super::sweep::{JanitorPolicy, SweepRequest, retention_duration, sweep};
use crate::bloomery::SourceShell;
use crate::bloomery::{
    CapturedObjects, LaneOccupancy, LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner,
};
use crate::store::{JournalWrite, OutstandingOrder, SqliteStore, StoreBackend};

fn journal(store: &mut SqliteStore, key: &str, fact: Fact) {
    let bytes = to_vec(&event(key, fact)).expect("the event encodes");
    store
        .append_event(&JournalWrite { idempotency_key: key, event: &bytes, decisions: &[], decider: "test" })
        .expect("the journal appends");
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
        self.released.0.lock().expect("the release log is not poisoned").push(worktree_dir.to_owned());
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

/// A host with no lane child in flight — every slot's target dir is fair game.
fn idle() -> LaneOccupancy {
    LaneOccupancy::default()
}

/// A host building in slot 0.
fn slot_zero_busy() -> LaneOccupancy {
    LaneOccupancy { slots: BTreeSet::from([0]), unattributed: false }
}

/// A re-adopted lane child whose slot this process could not recover, so no
/// target dir can be shown to be free.
fn busy_in_an_unknown_slot() -> LaneOccupancy {
    LaneOccupancy { slots: BTreeSet::new(), unattributed: true }
}

/// Plant a slot target directory holding `bytes` bytes, last written at `used`.
fn plant_target(base: &Path, slot: usize, bytes: usize, used: SystemTime) -> PathBuf {
    let dir = base.join(format!("slot-{slot}-target"));
    fs::create_dir_all(&dir).expect("the target dir is created");
    let cache = dir.join("cache");
    fs::write(&cache, vec![0_u8; bytes]).expect("the cache file writes");
    fs::File::open(&cache).expect("the cache file opens").set_modified(used).expect("the file mtime is set");
    // The directory last, because writing the file into it just bumped its own
    // mtime; `dir_usage` takes the newest stamp anywhere in the tree.
    fs::File::open(&dir).expect("the target dir opens").set_modified(used).expect("the dir mtime is set");
    dir
}

/// A fixed clock the recency fixtures are offset from, so ordering is pinned
/// rather than a function of how fast the suite ran.
fn epoch() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn run(request: &mut SweepRequest<'_>) -> super::sweep::SweepReport {
    sweep(request).expect("the sweep reads the in-memory store")
}

fn age_dir(path: &Path, now: SystemTime, days: u64) {
    let dest = now.checked_sub(retention_duration(days)).expect("the fixture clock sits after the retention window");
    fs::write(path.join(".aged"), []).expect("the age marker writes");
    fs::File::open(path).expect("the aged directory opens").set_modified(dest).expect("mtime is set");
}

#[test]
fn a_killed_runs_worktree_is_reclaimed_once_its_bloom_is_terminal() {
    // The motivating leak: the run ended by kill or crash, so the happy-path
    // release never ran, and the bloom has since been superseded. The journal
    // already knows the run is terminal; the sweep must reclaim the checkout
    // without a coordinator restart.
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let nonce = "killed-nonce";
    let worktree = scratch.path().join(nonce);
    fs::create_dir_all(&worktree).expect("the worktree is created");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
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
        lanes: &idle,
        policy: &keep,
        now,
    });

    assert_eq!(report.worktrees, 1, "the killed run's checkout is reclaimed");
    assert_eq!(released.0.lock().expect("the release log is not poisoned").as_slice(), [worktree]);
}

#[test]
fn a_lane_slots_checkout_is_never_reclaimed() {
    // Slot checkouts are the host's build paths, reused across dispatches.
    // Reclaiming one because no nonce names it would undo the cache layout
    // and pull the tree out from under whoever holds the slot.
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let worktree = scratch.path().join("slot-0");
    fs::create_dir_all(&worktree).expect("the worktree is created");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
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
        lanes: &idle,
        policy: &keep,
        now: SystemTime::now(),
    });

    assert_eq!(report.worktrees, 0);
    assert!(released.0.lock().expect("the release log is not poisoned").is_empty());
}

#[test]
fn a_live_blooms_outstanding_worktree_is_spared() {
    // A dispatch still outstanding against a sealed bloom is in flight, whether
    // or not its child is reachable. Reclaiming it is the opposite of the
    // kill/crash case: it would pull the tree out from under a live lane.
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let nonce = "live-nonce";
    let worktree = scratch.path().join(nonce);
    fs::create_dir_all(&worktree).expect("the worktree is created");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let bloom = journal_sealed(&mut store);
    store.record_order(&order_for(nonce, &bloom)).expect("the order is recorded");

    let released = Released(Arc::new(Mutex::new(Vec::new())));
    let runner = StubRunner { registered: vec![worktree], released: Released(Arc::clone(&released.0)) };
    let keep = policy(7, u64::MAX);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &idle,
        policy: &keep,
        now: SystemTime::now(),
    });

    assert_eq!(report.worktrees, 0, "a live bloom's outstanding checkout stays");
    assert!(released.0.lock().expect("the release log is not poisoned").is_empty());
}

#[test]
fn consumed_evidence_of_a_terminal_bloom_is_kept_inside_the_retention_window() {
    // ADR-0184's calibration ledger (and forensics) read study artefacts after
    // intake. Deleting a just-consumed directory the day the bloom lands would
    // silently drop those inputs inside the configured window.
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let nonce = "fresh-evidence";
    let evidence = scratch.path().join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence).expect("the evidence dir is created");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let (predecessor, _) = journal_superseded(&mut store);
    store.record_order(&order_for(nonce, &predecessor)).expect("the order is recorded");
    store.consume_order(nonce).expect("the order is consumed");

    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let now = SystemTime::now();
    let keep = policy(7, u64::MAX);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &idle,
        policy: &keep,
        now,
    });

    assert_eq!(report.evidence_dirs, 0);
    assert!(evidence.exists(), "evidence inside the 7-day window is kept");
}

#[test]
fn consumed_evidence_of_a_terminal_bloom_is_reclaimed_after_the_retention_window() {
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let nonce = "stale-evidence";
    let evidence = scratch.path().join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence).expect("the evidence dir is created");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let (predecessor, _) = journal_superseded(&mut store);
    store.record_order(&order_for(nonce, &predecessor)).expect("the order is recorded");
    store.consume_order(nonce).expect("the order is consumed");

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
        lanes: &idle,
        policy: &keep,
        now,
    });

    assert_eq!(report.evidence_dirs, 1);
    assert!(!evidence.exists(), "evidence past the window of a terminal bloom is reclaimed");
}

#[test]
fn target_dirs_are_swept_when_over_budget_and_idle() {
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let target = plant_target(scratch.path(), 0, 64, epoch());

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 16);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &idle,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 1);
    assert!(!target.exists(), "an idle host over budget drops the slot target dir it has to");
}

#[test]
fn an_over_budget_sweep_evicts_only_far_enough_to_get_under_budget() {
    // Tripwire: the sweep reclaimed *every* slot target dir on any overage,
    // which is what made the 2026-08-14 firing maximally destructive — three
    // warm caches deleted to recover the bytes of one. 192 bytes held against
    // a 150-byte budget needs one 64-byte eviction, not three.
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let coldest = plant_target(scratch.path(), 0, 64, epoch());
    let warmer = plant_target(scratch.path(), 1, 64, epoch() + Duration::from_mins(1));
    let warmest = plant_target(scratch.path(), 2, 64, epoch() + Duration::from_mins(2));

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 150);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &idle,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 1, "one eviction clears the overage");
    assert!(!coldest.exists(), "the least recently used slot goes first");
    assert!(warmer.exists() && warmest.exists(), "caches still inside the budget are kept");
}

#[test]
fn a_running_slots_target_dir_is_never_evicted() {
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let target = plant_target(scratch.path(), 0, 64, epoch());

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 16);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &slot_zero_busy,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 0);
    assert!(target.exists(), "a live lane's target dir is never swept, even over budget");
}

#[test]
fn an_idle_slot_is_evicted_while_its_busy_neighbour_is_spared() {
    // The reason occupancy is slot-keyed rather than a blanket "is anything
    // running": a host that always has some lane in flight would never enforce
    // its budget at all, which is how the incident host reached 100 GiB against
    // a 64 GiB line.
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let busy = plant_target(scratch.path(), 0, 64, epoch());
    let free = plant_target(scratch.path(), 1, 64, epoch() + Duration::from_mins(1));

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 16);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &slot_zero_busy,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 1);
    assert!(busy.exists(), "slot 0 is compiling");
    assert!(!free.exists(), "slot 1 is idle and the host is still over budget");
}

#[test]
fn a_lane_child_in_an_unrecoverable_slot_blocks_every_eviction() {
    // A boot re-adoption that recovered no slot record is still a live child
    // building somewhere. Reading that as "no slots occupied, sweep freely"
    // deletes the tree it is writing into.
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let target = plant_target(scratch.path(), 0, 64, epoch());

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 16);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &busy_in_an_unknown_slot,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 0);
    assert!(target.exists(), "an unattributable lane child protects every slot dir");
}

#[test]
fn a_slot_claimed_after_the_budget_was_measured_is_not_evicted() {
    // Tripwire for the 2026-08-14 mechanism: the guard was a `bool` sampled
    // once at the top of the tick, and the pass then replayed the journal,
    // pruned refs, and walked every target tree before acting on it. A slot
    // freed and re-claimed inside that window read as idle.
    //
    // This probe answers idle for the sweep's decision log and busy from then
    // on, standing in for a dispatch that claimed slot 0 while the pass was
    // measuring. Only an implementation that re-reads occupancy immediately
    // before each removal spares the directory.
    let claimed = AtomicBool::new(false);
    let lanes = || {
        if claimed.swap(true, Ordering::SeqCst) {
            slot_zero_busy()
        } else {
            idle()
        }
    };

    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let target = plant_target(scratch.path(), 0, 64, epoch());

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 16);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &lanes,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 0);
    assert!(target.exists(), "the sweep acted on a reading it took before the slot was claimed");
}

#[cfg(unix)]
#[test]
fn a_target_dir_that_could_not_be_removed_is_not_counted_and_is_retried() {
    // Tripwire: the removal loop counted attempts, so the incident logged
    // `target_dirs=2` beside `Directory not empty`. An operator reading that
    // line believed 50 GiB came back when none of it had.
    use std::os::unix::fs::PermissionsExt as _;

    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let stubborn = plant_target(scratch.path(), 0, 64, epoch());
    let removable = plant_target(scratch.path(), 1, 64, epoch() + Duration::from_mins(1));
    // Read+execute only: the tree can be renamed (that is the parent's
    // permission) but its entries cannot be unlinked, so `remove_dir_all`
    // fails partway exactly as a directory being written into does.
    fs::set_permissions(&stubborn, fs::Permissions::from_mode(0o500)).expect("the mode is set");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let runner = StubRunner { registered: vec![], released: Released(Arc::new(Mutex::new(Vec::new()))) };
    let tight = policy(7, 0);
    let report = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &idle,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(report.target_dirs, 1, "only the directory that actually went is counted");
    assert!(!removable.exists());
    let aside = moved_aside(scratch.path()).expect("the failed removal left its moved-aside tree behind");
    assert!(aside.join("cache").exists(), "the bytes are still there — the removal failed");
    assert!(!stubborn.exists(), "it is out of the build path, so no compile is hollowed out under it");

    // The next pass retries it rather than leaving a full target tree parked
    // under a name the budget no longer measures.
    fs::set_permissions(&aside, fs::Permissions::from_mode(0o700)).expect("the mode is restored");
    let retry = run(&mut SweepRequest {
        store: &mut store,
        runner: &runner,
        source: None,
        worktree_base: scratch.path(),
        target_base: scratch.path(),
        lanes: &idle,
        policy: &tight,
        now: SystemTime::now(),
    });

    assert_eq!(retry.target_dirs, 1);
    assert!(!aside.exists(), "a moved-aside tree is not left on the disk forever");
}

/// The single `slot-<index>-target.evicting-<stamp>` directory under `base`, if
/// the sweep left one.
fn moved_aside(base: &Path) -> Option<PathBuf> {
    fs::read_dir(base)
        .expect("the scratch root reads")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.contains(".evicting-")))
}

#[test]
fn terminal_bloom_working_refs_are_pruned_and_claim_refs_are_spared() {
    let scratch = tempfile::tempdir().expect("a scratch dir is created");
    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
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
        lanes: &idle,
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
