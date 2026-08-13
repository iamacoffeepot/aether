//! The runtime for the janitor reactor: a poll-driven loop that rebuilds the
//! snapshot from the journal and sweeps terminal artefacts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aether_actor::runtime;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::sweep::{JanitorPolicy, SweepRequest, sweep};
use super::{JanitorReactorCapability, JanitorReactorSetup};

use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::{
    CaptureIdentity, DEFAULT_LANE_PROGRAM, ExecutorShell, LaneProgram, ProcessTransformRunner, SourceShell,
    TransformRunner,
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
    policy: JanitorPolicy,
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
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            poll_interval_secs = config.poll_interval_secs,
            lane_target_budget_bytes = config.lane_target_budget_bytes,
            evidence_retention_days = config.evidence_retention_days,
            "janitor reactor mounted; sweeping terminal blooms on the coordinator cadence",
        );
        Ok(JanitorReactorState {
            source: config.source,
            executor: config.executor,
            store: Some(store),
            runner: Arc::new(ProcessTransformRunner::new(
                CaptureIdentity::default(),
                LaneProgram::parse(DEFAULT_LANE_PROGRAM),
            )),
            worktree_base,
            target_base,
            policy: JanitorPolicy {
                lane_target_budget_bytes: config.lane_target_budget_bytes,
                evidence_retention_days: config.evidence_retention_days,
            },
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
        let Some(store) = state.store.as_mut() else {
            return;
        };
        let lanes_running = state.executor.as_ref().is_some_and(ExecutorShell::any_lane_running);
        match sweep(&mut SweepRequest {
            store,
            runner: state.runner.as_ref(),
            source: state.source.as_ref(),
            worktree_base: &state.worktree_base,
            target_base: &state.target_base,
            lanes_running,
            policy: &state.policy,
            now: SystemTime::now(),
        }) {
            Ok(report) => {
                if report.worktrees + report.evidence_dirs + report.refs + report.target_dirs > 0 {
                    tracing::info!(
                        target: "aether_chassis_bloomery::janitor",
                        worktrees = report.worktrees,
                        evidence_dirs = report.evidence_dirs,
                        refs = report.refs,
                        target_dirs = report.target_dirs,
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
}
