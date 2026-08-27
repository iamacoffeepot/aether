//! The runtime for the janitor reactor: a poll-driven loop that rebuilds the
//! snapshot from the journal and sweeps terminal artefacts.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aether_actor::runtime;
use aether_bloomery::BloomId;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::archive::{ArchiveOutcome, ArchiveRequest, ArchiveTier, archive_pass};
use super::kinds::{
    ArchiveFailureView, ArchiveRecords, ArchiveRecordsResult, ArchivedRecordView, ListArchive, ListArchiveResult,
};
use super::sweep::{JanitorPolicy, SweepRequest, TargetScan, WorkingRefPruner, sweep};
use super::{JanitorReactorCapability, JanitorReactorSetup};

use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::{
    CaptureIdentity, DEFAULT_LANE_PROGRAM, ExecutorShell, LaneOccupancy, LaneProgram, ProcessTransformRunner,
    SourceShell, TransformRunner,
};
use crate::store::SqliteStore;

/// The self-addressed wake the poll timer fires each interval.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.janitor.janitor_tick")]
pub struct JanitorTick {}

/// Runtime state for [`JanitorReactorCapability`].
pub struct JanitorReactorState {
    source: Option<SourceShell>,
    executor: Option<ExecutorShell>,
    store: Option<SqliteStore>,
    runner: Arc<dyn TransformRunner>,
    worktree_base: PathBuf,
    target_base: PathBuf,
    tier: ArchiveTier,
    policy: JanitorPolicy,
    /// Free-slot set and stamp the last size walk was taken against. Reused
    /// across ticks until occupancy changes or the scan interval elapses.
    scan: TargetScan,
    /// Blooms whose working refs this process has already pruned successfully.
    /// Process-local: a restart walks the terminal set again, one bloom per
    /// tick, and a prune that errors is left out so the next tick retries.
    pruned: HashSet<BloomId>,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    _timer: Option<TimerHandle>,
}

#[runtime]
impl NativeActor for JanitorReactorCapability {
    type State = JanitorReactorState;
    type Config = ();
    type Params = JanitorReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.janitor";

    fn init(
        (): (),
        config: JanitorReactorSetup,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<JanitorReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let store = SqliteStore::open(&config.store_path).map_err(|e| BootError::Other(Box::new(e)))?;
        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            JanitorTick::ID,
            JanitorTick::default().encode_into_bytes(),
            "aether-bloomery-janitor",
            interval,
        );
        let worktree_base = PathBuf::from(&config.worktree_base);
        let target_base = if config.target_base.is_empty() {
            worktree_base.clone()
        } else {
            PathBuf::from(config.target_base)
        };
        let archive_base = if config.archive_base.is_empty() {
            worktree_base.join("archive")
        } else {
            PathBuf::from(config.archive_base)
        };
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            poll_interval_secs = config.poll_interval_secs,
            lane_target_budget_bytes = config.lane_target_budget_bytes,
            target_scan_interval_secs = config.target_scan_interval_secs,
            evidence_retention_days = config.evidence_retention_days,
            archive_base = %archive_base.display(),
            "janitor reactor mounted; sweeping working state on the coordinator cadence; evidence archives after the retention window",
        );
        Ok(JanitorReactorState {
            source: config.source,
            executor: config.executor,
            store: Some(store),
            runner: Arc::new(ProcessTransformRunner::new(
                CaptureIdentity::default(),
                LaneProgram::parse(DEFAULT_LANE_PROGRAM),
                PathBuf::from(&config.repo),
            )),
            worktree_base,
            target_base,
            tier: ArchiveTier::new(archive_base),
            policy: JanitorPolicy {
                lane_target_budget_bytes: config.lane_target_budget_bytes,
                target_scan_interval_secs: config.target_scan_interval_secs,
                evidence_retention_days: config.evidence_retention_days,
            },
            scan: TargetScan::default(),
            pruned: HashSet::new(),
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot tick so a kill or crash left unreclaimed by the
    /// previous process is swept without waiting a full poll interval.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        state.mailer.push(Mail::new(
            state.self_mailbox,
            JanitorTick::ID,
            JanitorTick::default().encode_into_bytes(),
            1,
        ));
    }

    #[handler::single]
    fn on_janitor_tick(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: JanitorTick) {
        // Cloned rather than borrowed so the probe outlives the `&mut` the
        // store binding below takes on the same state. An `ExecutorShell` is
        // two `Arc`s; the clone shares the very registry the lanes claim slots
        // in, which is the whole point of asking it live.
        let executor = state.executor.clone();
        let lanes = || executor.as_ref().map_or_else(LaneOccupancy::default, ExecutorShell::lane_occupancy);

        let Some(store) = state.store.as_mut() else {
            return;
        };
        match sweep(
            &mut SweepRequest {
                store,
                runner: state.runner.as_ref(),
                source: state.source.as_ref().map(|shell| shell as &dyn WorkingRefPruner),
                worktree_base: &state.worktree_base,
                target_base: &state.target_base,
                lanes: &lanes,
                policy: &state.policy,
                now: SystemTime::now(),
                pruned: &mut state.pruned,
            },
            &mut state.scan,
        ) {
            Ok(report) => {
                if report.worktrees + report.refs + report.target_dirs > 0 {
                    tracing::info!(
                        target: "aether_chassis_bloomery::janitor",
                        worktrees = report.worktrees,
                        refs = report.refs,
                        target_dirs = report.target_dirs,
                        targets_measured = report.targets_measured,
                        "janitor: reclaimed terminal artefacts",
                    );
                }
            }
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                %error,
                "janitor: sweep failed; will retry next tick",
            ),
        }
    }

    #[handler::single]
    fn on_archive_records(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: ArchiveRecords,
    ) -> ArchiveRecordsResult {
        let Some(store) = state.store.as_mut() else {
            return ArchiveRecordsResult::Errored { error: "janitor store is unavailable".to_owned() };
        };
        match archive_pass(&mut ArchiveRequest {
            store,
            runner: state.runner.as_ref(),
            worktree_base: &state.worktree_base,
            tier: &state.tier,
            policy: &state.policy,
            now: SystemTime::now(),
        }) {
            Ok(ArchiveOutcome::Archived { records, failures }) => ArchiveRecordsResult::Archived {
                records: records.into_iter().map(record_view).collect(),
                failures: failures
                    .into_iter()
                    .map(|failure| ArchiveFailureView {
                        class: failure.class,
                        name: failure.name,
                        error: failure.error,
                    })
                    .collect(),
            },
            Ok(ArchiveOutcome::Refused { reason }) => ArchiveRecordsResult::Refused { reason },
            Err(error) => ArchiveRecordsResult::Errored { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_list_archive(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListArchive) -> ListArchiveResult {
        match state.tier.list() {
            Ok(records) => ListArchiveResult::Ok { records: records.into_iter().map(record_view).collect() },
            Err(error) => ListArchiveResult::Err { error: error.to_string() },
        }
    }
}

fn record_view(record: super::archive::ArchivedRecord) -> ArchivedRecordView {
    ArchivedRecordView {
        class: record.class.as_str().to_owned(),
        name: record.name,
        path: record.path.display().to_string(),
        bytes: record.bytes,
    }
}
