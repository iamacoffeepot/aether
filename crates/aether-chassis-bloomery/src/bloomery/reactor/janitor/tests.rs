//! The journal-driven sweep, over a real `SqliteStore`, a stub-runner
//! [`LocalExecutor`], and a fake-GitHub-backed `SourceShell` — the kill/crash
//! leak this reactor exists to close, and the retention / budget / ref-prune
//! policies that sit beside it.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aether_bloomery::{
    BloomDraft, BloomId, BloomStatus, Digest, Event, Evidence, EvidenceKind, ExecutorBackend, Fact, IdempotencyKey,
    Membership, Nonce, ResolvedConfigs, Snapshot, StageCatalog, StageId, Transformation, WorkOrder, WorkpieceId,
    is_active_unlanded, reduce,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GitSource, MainlineRef, candidate_ref_name, short_hex};
use aether_data::wire::{from_bytes, to_vec};
use tempfile::TempDir;

use super::runtime::{SweepPolicy, sweep};
use crate::bloomery::SourceShell;
use crate::bloomery::{
    CapturedObjects, LocalExecutor, LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner,
};
use crate::store::{OutstandingOrder, SqliteStore, StoreBackend};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn correspondence() -> Arc<FakeGithub> {
    let fake = FakeGithub::new();
    fake.seed_git_object(&digest(0xC0));
    fake.seed_git_object(&digest(0xB0));
    Arc::new(fake)
}

fn policy(retain_days: u64, budget_bytes: u64) -> SweepPolicy {
    SweepPolicy { lane_target_budget_bytes: budget_bytes, evidence_retain_days: retain_days, lane_scratch: None }
}

fn member(name: &str, scope: u8) -> Membership {
    let mut membership = Membership {
        workpiece: WorkpieceId(name.to_owned()),
        scope_revision: digest(scope),
        configs: aether_bloomery::ConfigRegistry::default(),
        approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail: digest(3) },
    };
    membership.approval.subject = membership.subject();
    membership
}

fn spec_with(base: u8, name: &str, scope: u8) -> aether_bloomery::BloomSpec {
    BloomDraft { proposals: vec![member(name, scope)], base: digest(base), ..BloomDraft::default() }.seal()
}

fn journal_seal_then_supersede(store: &mut SqliteStore) -> (BloomId, BloomId) {
    let predecessor_spec = spec_with(0, "wp-a", 2);
    let predecessor = predecessor_spec.id();
    let successor_spec = spec_with(0, "wp-a", 4);
    let successor = successor_spec.id();

    let seal = Event { idempotency_key: IdempotencyKey("seal".to_owned()), fact: Fact::Seal(predecessor_spec) };
    store.append_event(&seal.idempotency_key.0, &to_vec(&seal).expect("seal encodes")).expect("seal journals");

    let supersede = Event {
        idempotency_key: IdempotencyKey("supersede".to_owned()),
        fact: Fact::Supersede { predecessor, successor: successor_spec },
    };
    store
        .append_event(&supersede.idempotency_key.0, &to_vec(&supersede).expect("supersede encodes"))
        .expect("supersede journals");

    (predecessor, successor)
}

fn snapshot_of(store: &mut SqliteStore) -> Snapshot {
    let mut snapshot = Snapshot::default();
    let configs = ResolvedConfigs::default();
    for record in store.replay_journal().expect("journal reads") {
        let event: Event = from_bytes(&record.event).expect("event decodes");
        let decisions = reduce(&snapshot, &event, &configs);
        snapshot = snapshot.apply(&event, &decisions, &configs);
    }
    snapshot
}

#[derive(Clone, Default)]
struct RunLog {
    released: Arc<Mutex<Vec<PathBuf>>>,
}

struct RecordingRunner {
    registered: Vec<PathBuf>,
    log: RunLog,
}

impl TransformRunner for RecordingRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        Ok(Box::new(IdleProcess))
    }

    fn release(&self, worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        self.log.released.lock().unwrap().push(worktree_dir.to_owned());
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

struct IdleProcess;

impl RunProcess for IdleProcess {
    fn poll(&mut self) -> RunLifecycle {
        RunLifecycle::Running
    }

    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        Ok(())
    }
}

fn sweeping_executor(base: &TempDir, registered: Vec<PathBuf>) -> (LocalExecutor, RunLog) {
    let log = RunLog::default();
    let runner = RecordingRunner { registered, log: log.clone() };
    (LocalExecutor::new(Arc::new(runner), correspondence(), base.path()), log)
}

fn shell(fake: FakeGithub) -> SourceShell {
    SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake), true, MainlineRef::default())))
}

#[test]
fn a_killed_run_s_leftover_worktree_is_reclaimed_without_a_restart() {
    // The leak that motivated this reactor: the backend releases a worktree only
    // on a run's clean terminal path, so a kill or crash leaves the nonce-keyed
    // checkout on disk until a coordinator restart. The sweeper reads the
    // journal's outstanding set, not the happy-path release, so a dir whose
    // owning run is gone is swept regardless of how it ended.
    let base = TempDir::new().unwrap();
    let live = "n-live";
    let crashed = "n-crashed";
    for nonce in [live, crashed] {
        fs::create_dir_all(base.path().join(nonce)).unwrap();
        fs::create_dir_all(base.path().join(format!("{nonce}-evidence"))).unwrap();
    }
    let registered = vec![base.path().join(live), base.path().join(crashed)];
    let (local, log) = sweeping_executor(&base, registered);

    let mut store = SqliteStore::open(":memory:").unwrap();
    store
        .record_order(&OutstandingOrder {
            nonce: live.to_owned(),
            bloom: digest(1).as_bytes().to_vec(),
            workpiece: "wp".to_owned(),
            scope_revision: digest(2).as_bytes().to_vec(),
            candidate: digest(3).as_bytes().to_vec(),
            displayed_digest: digest(3).as_bytes().to_vec(),
            stage: Vec::new(),
            transformation: Vec::new(),
            configs: Vec::new(),
            profile: Vec::new(),
            deadline_unix_millis: 0,
        })
        .unwrap();

    let report = sweep(&mut store, Some(&local), None, &policy(7, 0), SystemTime::now()).unwrap();

    assert_eq!(report.abandoned, 1, "the crashed run's checkout is reclaimed");
    assert_eq!(log.released.lock().unwrap().as_slice(), [base.path().join(crashed)]);
    assert!(base.path().join(live).exists(), "the live order's checkout is untouched");
    assert!(base.path().join(format!("{live}-evidence")).exists(), "so is its evidence");
    assert!(!base.path().join(format!("{crashed}-evidence")).exists(), "the crashed run's evidence pair goes with it");
}

#[test]
fn a_superseded_bloom_s_ephemeral_refs_are_pruned_and_its_claim_refs_are_spared() {
    // Claim refs already have their release reactor (ADR-0150 / ADR-0179). This
    // sweep covers the candidate / integration / checkpoint names that otherwise
    // outlive the bloom indefinitely — and must not reach across into the claim
    // namespace to do it.
    let fake = FakeGithub::new();
    let source = shell(fake.clone());
    let mut store = SqliteStore::open(":memory:").unwrap();
    let (predecessor, successor) = journal_seal_then_supersede(&mut store);

    let snapshot = snapshot_of(&mut store);
    assert_eq!(snapshot.blooms.get(&predecessor).unwrap().status, BloomStatus::Superseded);
    assert!(is_active_unlanded(snapshot.blooms.get(&successor).unwrap().status));

    assert_eq!(
        source.claim_seal(&successor, &[WorkpieceId("wp-a".to_owned())]).unwrap(),
        aether_bloomery::ClaimOutcome::Acquired
    );

    let sha = "aa".repeat(20);
    fake.seed_ref(candidate_ref_name(&predecessor, "wp-a").trim_start_matches("refs/"), &sha);
    fake.seed_ref(&format!("heads/bloom/{}/integration", short_hex(&predecessor.0)), &sha);
    fake.seed_ref(&format!("heads/bloom/{}/checkpoint/{}", short_hex(&predecessor.0), "bb".repeat(32)), &sha);
    fake.seed_ref(candidate_ref_name(&successor, "wp-a").trim_start_matches("refs/"), &sha);

    let report = sweep(&mut store, None, Some(&source), &policy(7, 0), SystemTime::now()).unwrap();

    assert_eq!(report.refs, 1, "one terminal bloom is pruned");
    assert!(
        !fake.ref_exists(candidate_ref_name(&predecessor, "wp-a").trim_start_matches("refs/")),
        "predecessor candidate is gone"
    );
    assert!(
        !fake.ref_exists(&format!("heads/bloom/{}/integration", short_hex(&predecessor.0))),
        "predecessor integration is gone",
    );
    assert!(
        fake.ref_exists(candidate_ref_name(&successor, "wp-a").trim_start_matches("refs/")),
        "the live successor's candidate is untouched",
    );
    assert_eq!(
        source.enumerate_claims().unwrap().len(),
        2,
        "the successor's claim refs (member + admission) survive — they are not this sweep's",
    );
}

#[test]
fn consumed_evidence_of_a_live_bloom_is_kept_inside_the_retention_window() {
    // Evidence feeds intake and then forensics / the calibration ledger's
    // inputs. A silent delete of a live bloom's evidence — or of a terminal
    // bloom's evidence still inside the window — is the failure this policy
    // exists to prevent. ADR-0184 reads study artifacts from the artifacts
    // store, which this sweep never walks.
    let base = TempDir::new().unwrap();
    let mut store = SqliteStore::open(":memory:").unwrap();
    let (predecessor, successor) = journal_seal_then_supersede(&mut store);

    let live_dir = base.path().join("n-live-evidence");
    let terminal_fresh = base.path().join("n-terminal-fresh-evidence");
    let terminal_old = base.path().join("n-terminal-old-evidence");
    for dir in [&live_dir, &terminal_fresh, &terminal_old] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("evidence.json"), "{}").unwrap();
    }
    fs::write(live_dir.join("bloom"), hex_of(successor.0.as_bytes())).unwrap();
    fs::write(terminal_fresh.join("bloom"), hex_of(predecessor.0.as_bytes())).unwrap();
    fs::write(terminal_old.join("bloom"), hex_of(predecessor.0.as_bytes())).unwrap();

    let old = SystemTime::now() - Duration::from_hours(240);
    fs::File::open(&terminal_old).unwrap().set_modified(old).unwrap();

    let (local, _) = sweeping_executor(&base, Vec::new());
    let report = sweep(&mut store, Some(&local), None, &policy(7, 0), SystemTime::now()).unwrap();

    assert_eq!(report.evidence, 1, "only the terminal bloom's aged evidence is reclaimed");
    assert!(live_dir.exists(), "a live bloom's evidence is never deleted inside the window");
    assert!(terminal_fresh.exists(), "a terminal bloom's fresh evidence stays for the retention window");
    assert!(!terminal_old.exists(), "a terminal bloom's evidence past the window is reclaimed");
}

#[test]
fn lane_targets_sweep_when_over_budget_and_idle_never_while_a_lane_runs() {
    let base = TempDir::new().unwrap();
    let target = base.path().join("slot-0-target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("blob"), vec![0u8; 64]).unwrap();

    let (idle, _) = sweeping_executor(&base, Vec::new());
    let mut store = SqliteStore::open(":memory:").unwrap();
    let swept = sweep(&mut store, Some(&idle), None, &policy(7, 8), SystemTime::now()).unwrap();
    assert_eq!(swept.targets, 1, "over budget and idle: the slot target is swept");
    assert!(!target.exists(), "the over-budget tree is gone");

    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("blob"), vec![0u8; 64]).unwrap();
    let (busy, _) = sweeping_executor(&base, Vec::new());
    busy.submit(&WorkOrder {
        transformation: Transformation::for_member_stage(
            &StageCatalog::binding_of(StageId::Construct),
            digest(5),
            digest(0xC0),
            digest(0xB0),
        ),
        nonce: Nonce("n-busy".to_owned()),
    })
    .unwrap();
    assert!(busy.occupied_lanes() > 0, "the submitted run occupies a lane");

    let held = sweep(&mut store, Some(&busy), None, &policy(7, 8), SystemTime::now()).unwrap();
    assert_eq!(held.targets, 0, "a running lane blocks the budget sweep");
    assert!(target.exists(), "the tree a live cargo may still hold stays");
}

fn hex_of(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

#[test]
fn lane_scratch_of_a_consumed_run_is_reclaimed() {
    // Model-lane throwaway trees live under AETHER_LANE_SCRATCH and were only
    // reaped by the unit's ExecStartPre. A coordinator that runs for a week
    // never hit that path; the janitor does, keyed by the same outstanding set.
    let base = TempDir::new().unwrap();
    let scratch = base.path().join("scratch-root");
    fs::create_dir_all(scratch.join("scratch-n-dead")).unwrap();
    fs::create_dir_all(scratch.join("scratch-n-live")).unwrap();
    fs::write(scratch.join("not-a-run"), b"keep").unwrap();

    let (local, _) = sweeping_executor(&base, Vec::new());
    let mut store = SqliteStore::open(":memory:").unwrap();
    store
        .record_order(&OutstandingOrder {
            nonce: "n-live".to_owned(),
            bloom: digest(1).as_bytes().to_vec(),
            workpiece: "wp".to_owned(),
            scope_revision: digest(2).as_bytes().to_vec(),
            candidate: digest(3).as_bytes().to_vec(),
            displayed_digest: digest(3).as_bytes().to_vec(),
            stage: Vec::new(),
            transformation: Vec::new(),
            configs: Vec::new(),
            profile: Vec::new(),
            deadline_unix_millis: 0,
        })
        .unwrap();

    let mut policy = policy(7, 0);
    policy.lane_scratch = Some(scratch.clone());
    let report = sweep(&mut store, Some(&local), None, &policy, SystemTime::now()).unwrap();

    assert_eq!(report.scratch, 1, "only the consumed run's scratch is reclaimed");
    assert!(!scratch.join("scratch-n-dead").exists(), "the dead run's throwaway tree is gone");
    assert!(scratch.join("scratch-n-live").exists(), "a live run's tree is untouched");
    assert!(scratch.join("not-a-run").exists(), "a file the backend did not name is not this sweep's");
}

#[test]
fn a_torn_journal_skips_the_sweep_rather_than_deleting_against_guesswork() {
    let base = TempDir::new().unwrap();
    fs::create_dir_all(base.path().join("n-x")).unwrap();
    let (local, log) = sweeping_executor(&base, vec![base.path().join("n-x")]);
    let mut store = SqliteStore::open(":memory:").unwrap();
    store.append_event("bad", b"not-an-event").unwrap();

    let report = sweep(&mut store, Some(&local), None, &policy(0, 0), SystemTime::now()).unwrap();
    assert_eq!(report, super::runtime::SweepReport::default(), "a torn journal reclaims nothing");
    assert!(log.released.lock().unwrap().is_empty(), "the stub runner was not asked to release");
    assert!(base.path().join("n-x").exists(), "the leftover stays until the journal can name it");
}
