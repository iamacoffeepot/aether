//! The poll-driven doctor pass: rebuild live state, evaluate the seed
//! invariants, publish the report to `/view`, and post new violations through
//! the operator alert channel.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use aether_actor::runtime;
use aether_bloomery::{
    BackendObjectId, BloomId, BloomStatus, Digest, Event, Fact, ResolvedConfigs, SharedCorrespondence, Snapshot, Topic,
    decode_recorded_decisions,
};
use aether_bloomery_github::GitObjectId;
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::invariants::{DoctorReport, LiveState, OpenDispatch, ReplicaObservation, evaluate};
use super::{DoctorReactorCapability, DoctorReactorSetup};
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::{ExecutorShell, SourceShell};
use crate::store::{OutboxEntry, SqliteStore, StoreBackend};

/// The self-addressed wake the poll timer fires each interval.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.doctor.doctor_tick")]
pub struct DoctorTick {}

/// Shared latest report the REST `/view` overlay reads.
///
/// Cloning shares the same cell: the reactor writes, the API reads. A
/// violation that only appears in journald is a failure of this design, so
/// the board is the operator channel `/view` projects.
#[derive(Clone, Default)]
pub struct DoctorBoard {
    inner: Arc<Mutex<BoardInner>>,
}

#[derive(Default)]
struct BoardInner {
    report: Option<DoctorReport>,
    last_fingerprint: String,
}

impl DoctorBoard {
    /// The latest completed pass, if one has run.
    #[must_use]
    pub fn latest(&self) -> Option<DoctorReport> {
        self.lock().report.clone()
    }

    /// Record `report` and post newly-loud violations through the operator
    /// alert channel. Idempotent on the same failing set: a stable violation
    /// is not re-posted every poll.
    pub fn publish(&self, report: DoctorReport) {
        let fingerprint = report.fingerprint();
        let mut inner = self.lock();
        let changed = fingerprint != inner.last_fingerprint;
        if changed && !report.is_clean() {
            for check in report.violations() {
                tracing::error!(
                    target: "aether_chassis_bloomery::doctor::alert",
                    invariant = check.name,
                    statement = check.statement,
                    divergences = %check.divergences.join("; "),
                    "doctor invariant violated",
                );
            }
        }
        inner.last_fingerprint = fingerprint;
        inner.report = Some(report);
    }

    fn lock(&self) -> MutexGuard<'_, BoardInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runtime state for [`DoctorReactorCapability`].
pub struct DoctorReactorState {
    source: Option<SourceShell>,
    executor: Option<ExecutorShell>,
    correspondence: Option<SharedCorrespondence>,
    store: Option<SqliteStore>,
    worktree_base: PathBuf,
    board: DoctorBoard,
    replica_seen: BTreeMap<u64, Instant>,
    replica_passes: BTreeMap<u64, u32>,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    _timer: Option<TimerHandle>,
}

#[runtime]
impl NativeActor for DoctorReactorCapability {
    type State = DoctorReactorState;
    type Config = ();
    type Params = DoctorReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.doctor";

    fn init((): (), config: DoctorReactorSetup, ctx: &mut NativeInitCtx<'_>) -> Result<DoctorReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let store = SqliteStore::open(&config.store_path).map_err(|error| BootError::Other(Box::new(error)))?;
        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            DoctorTick::ID,
            DoctorTick::default().encode_into_bytes(),
            "aether-bloomery-doctor",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::doctor",
            poll_interval_secs = config.poll_interval_secs,
            "doctor mounted; evaluating cross-source invariants on the coordinator cadence",
        );
        Ok(DoctorReactorState {
            source: config.source,
            executor: config.executor,
            correspondence: config.correspondence,
            store: Some(store),
            worktree_base: PathBuf::from(config.worktree_base),
            board: config.board,
            replica_seen: BTreeMap::new(),
            replica_passes: BTreeMap::new(),
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot pass so a stranded claim ref or drifted head is
    /// loud before the first poll interval, once the journal is readable.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        state.mailer.push(Mail::new(state.self_mailbox, DoctorTick::ID, DoctorTick::default().encode_into_bytes(), 1));
    }

    #[handler::single]
    fn on_doctor_tick(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DoctorTick) {
        let executor = state.executor.clone();
        let lanes_running = executor.as_ref().is_some_and(|shell| shell.lane_occupancy().any_running());
        let Some(store) = state.store.as_mut() else {
            return;
        };
        match collect_and_evaluate(&mut CollectRequest {
            store,
            source: state.source.as_ref(),
            correspondence: state.correspondence.as_ref(),
            worktree_base: &state.worktree_base,
            lanes_running,
            replica_seen: &mut state.replica_seen,
            replica_passes: &mut state.replica_passes,
            now: Instant::now(),
        }) {
            Ok(report) => state.board.publish(report),
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                %error,
                "doctor pass failed to collect live state",
            ),
        }
    }
}

struct CollectRequest<'a> {
    store: &'a mut dyn StoreBackend,
    source: Option<&'a SourceShell>,
    correspondence: Option<&'a SharedCorrespondence>,
    worktree_base: &'a Path,
    lanes_running: bool,
    replica_seen: &'a mut BTreeMap<u64, Instant>,
    replica_passes: &'a mut BTreeMap<u64, u32>,
    now: Instant,
}

fn collect_and_evaluate(request: &mut CollectRequest<'_>) -> rusqlite::Result<DoctorReport> {
    let (snapshot, landed_heads) = replay(request.store)?;
    let outstanding_rows = outstanding(request.store)?;
    let outstanding: Vec<OpenDispatch<'_>> = outstanding_rows
        .iter()
        .map(|row| OpenDispatch { nonce: row.nonce.as_str(), workpiece: row.workpiece.as_str() })
        .collect();
    let evidence = evidence_nonces(request.worktree_base);
    let evidence_refs: Vec<&str> = evidence.iter().map(String::as_str).collect();
    let claims = request.source.map(SourceShell::enumerate_claims).transpose().unwrap_or_else(|error| {
        tracing::warn!(
            target: "aether_chassis_bloomery::doctor",
            %error,
            "doctor could not enumerate claim refs",
        );
        None
    });
    let claims = claims.unwrap_or_default();
    let (actual_head, actual_head_sha) = actual_daily_head(request.source, request.correspondence);
    let pairs = request.correspondence.as_ref().map_or_else(Vec::new, |store| match store.pairs() {
        Ok(pairs) => pairs,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                %error,
                "doctor could not list correspondence",
            );
            Vec::new()
        }
    });
    let replica_topics = request.store.drain_outbox(Some(Topic::SourceReplica.as_str()))?;
    let replica = observe_replica(&replica_topics, request.replica_seen, request.replica_passes, request.now);
    let ancestry = |from: &Digest, to: &Digest| request.source.and_then(|source| source.is_fast_forward(from, to).ok());

    Ok(evaluate(&LiveState {
        snapshot: &snapshot,
        claims: &claims,
        actual_head,
        actual_head_sha: actual_head_sha.as_deref(),
        correspondence: &pairs,
        landed_heads: &landed_heads,
        ancestry: Some(&ancestry),
        replica: &replica,
        outstanding: &outstanding,
        lanes_running: request.lanes_running,
        evidence_nonces: &evidence_refs,
    }))
}

fn replay(store: &mut dyn StoreBackend) -> rusqlite::Result<(Snapshot, Vec<(BloomId, Digest)>)> {
    let mut configs = ResolvedConfigs::default();
    for record in store.load_configs()? {
        let Some(address) = Digest::from_slice(&record.digest) else {
            continue;
        };
        configs.insert(address, record.kind, record.bytes);
    }

    let mut snapshot = Snapshot::default();
    let mut landed = Vec::new();
    for record in store.replay_journal()? {
        let Ok(event) = from_bytes::<Event>(&record.event) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                sequence = record.sequence,
                "doctor: journal event did not decode; skipping",
            );
            continue;
        };
        let Ok(decisions) = decode_recorded_decisions(&record.decisions, record.decisions_schema.as_deref()) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                sequence = record.sequence,
                "doctor: journal decisions did not decode; skipping",
            );
            continue;
        };
        snapshot = snapshot.apply(&event, &decisions, &configs);
        if let Fact::Land { bloom, new_head } = event.fact
            && snapshot.blooms.get(&bloom).is_some_and(|record| record.status == BloomStatus::Landed)
        {
            landed.retain(|(id, _)| id != &bloom);
            landed.push((bloom, new_head));
        }
    }
    Ok((snapshot, landed))
}

struct OutstandingRow {
    nonce: String,
    workpiece: String,
}

fn outstanding(store: &mut dyn StoreBackend) -> rusqlite::Result<Vec<OutstandingRow>> {
    let mut rows = Vec::new();
    for nonce in store.list_outstanding_nonces()? {
        let Some(order) = store.lookup_order(&nonce)? else {
            continue;
        };
        rows.push(OutstandingRow { nonce: order.nonce, workpiece: order.workpiece });
    }
    Ok(rows)
}

fn evidence_nonces(worktree_base: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(worktree_base) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            entry.file_name().to_str().and_then(|name| name.strip_suffix("-evidence")).map(str::to_owned)
        })
        .collect()
}

fn actual_daily_head(
    source: Option<&SourceShell>,
    correspondence: Option<&SharedCorrespondence>,
) -> (Option<Digest>, Option<String>) {
    let Some(source) = source else {
        return (None, None);
    };
    let sha = match source.mainline_head_sha() {
        Ok(sha) => sha,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::doctor",
                %error,
                "doctor could not read the daily ref head",
            );
            return (None, None);
        }
    };
    let digest = correspondence.and_then(|store| {
        let object = GitObjectId::from_hex(&sha).map(BackendObjectId::from)?;
        match store.resolve_digest(&object) {
            Ok(digest) => digest,
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::doctor",
                    %error,
                    "doctor could not resolve the daily ref through correspondence",
                );
                None
            }
        }
    });
    (digest, Some(sha))
}

fn observe_replica(
    entries: &[OutboxEntry],
    seen: &mut BTreeMap<u64, Instant>,
    passes: &mut BTreeMap<u64, u32>,
    now: Instant,
) -> Vec<ReplicaObservation> {
    let live: BTreeMap<u64, Instant> =
        entries.iter().map(|entry| (entry.sequence, *seen.get(&entry.sequence).unwrap_or(&now))).collect();
    seen.retain(|sequence, _| live.contains_key(sequence));
    passes.retain(|sequence, _| live.contains_key(sequence));
    for (sequence, first) in &live {
        seen.insert(*sequence, *first);
        *passes.entry(*sequence).or_insert(0) += 1;
    }
    live.into_iter()
        .map(|(sequence, first)| ReplicaObservation {
            sequence,
            age: now.saturating_duration_since(first),
            consecutive_failures: passes.get(&sequence).copied().unwrap_or(1),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{CheckResult, DoctorReport};
    use super::DoctorBoard;

    #[test]
    fn publish_keeps_the_latest_report_for_view() {
        // The plausible bug: the board stores the report but /view cannot
        // read it, so a violation is only a log line.
        let board = DoctorBoard::default();
        assert!(board.latest().is_none());
        board.publish(DoctorReport {
            checks: vec![CheckResult {
                name: "observed_head_equals_daily_head",
                statement: "the observed head equals the actual daily ref head",
                passed: false,
                divergences: vec!["observed aa != actual bb".into()],
            }],
        });
        let latest = board.latest().expect("a published report is readable");
        assert!(!latest.is_clean());
        assert_eq!(latest.named("observed_head_equals_daily_head").map(|check| check.passed), Some(false));
    }
}
