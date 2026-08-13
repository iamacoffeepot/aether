//! The runtime for the janitor reactor: replay the journal, reclaim what
//! terminal state names.
//!
//! Each tick rebuilds the snapshot from the store, lists outstanding nonces,
//! and sweeps leftover worktrees, consumed evidence (after the retention
//! window), lane scratch, idle-and-over-budget target dirs, and ephemeral
//! repository refs. A decode fault skips the tick rather than sweeping against
//! a torn snapshot.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aether_actor::runtime;
use aether_bloomery::{BloomId, BloomStatus, Digest, Event, ResolvedConfigs, Snapshot, is_active_unlanded, reduce};
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::{JanitorReactorCapability, JanitorReactorSetup};

use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::{LocalExecutor, SourceShell};
use crate::store::{SqliteStore, StoreBackend};

/// The self-addressed wake the poll timer fires each interval. Zero-field —
/// the timer carries only the schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.janitor.janitor_tick")]
pub struct JanitorTick {}

/// The retention and budget knobs one sweep pass applies.
#[derive(Clone, Debug)]
pub struct SweepPolicy {
    /// Combined ceiling across every per-slot cargo target directory, in bytes.
    /// `0` disables the budget sweep.
    pub lane_target_budget_bytes: u64,
    /// Days to keep consumed evidence after its bloom is terminal. `0` reclaims
    /// on this pass. Live blooms' evidence is never deleted.
    pub evidence_retain_days: u64,
    /// Root of the per-run throwaway build trees. `None` skips that sweep.
    pub lane_scratch: Option<PathBuf>,
}

/// What one sweep pass reclaimed, for the log line and for tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Nonce-keyed leftover checkouts (and their evidence siblings) belonging
    /// to no live order — the kill/crash leak.
    pub abandoned: usize,
    /// Consumed evidence directories past the retention window.
    pub evidence: usize,
    /// `scratch-<nonce>` leftovers under the configured scratch root.
    pub scratch: usize,
    /// Per-slot cargo target directories removed after an idle over-budget.
    pub targets: usize,
    /// Terminal blooms whose ephemeral repository refs were pruned.
    pub refs: usize,
}

/// Runtime state for [`JanitorReactorCapability`].
pub struct JanitorReactorState {
    local: Option<Arc<LocalExecutor>>,
    source: Option<SourceShell>,
    store: Option<SqliteStore>,
    policy: SweepPolicy,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    _timer: Option<TimerHandle>,
}

/// Replay the journal into a snapshot. `None` when a record does not decode —
/// the caller skips the tick rather than sweeping against a torn projection.
fn replay_snapshot(store: &mut dyn StoreBackend) -> rusqlite::Result<Option<Snapshot>> {
    let mut configs = ResolvedConfigs::default();
    for record in store.load_configs()? {
        let Some(address) = Digest::from_slice(&record.digest) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                "janitor: stored configuration address did not decode; skipping this sweep",
            );
            return Ok(None);
        };
        configs.insert(address, record.kind, record.bytes);
    }

    let mut snapshot = Snapshot::default();
    for record in store.replay_journal()? {
        let Ok(event) = from_bytes::<Event>(&record.event) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                sequence = record.sequence,
                "janitor: journal record did not decode; skipping this sweep",
            );
            return Ok(None);
        };
        let decisions = reduce(&snapshot, &event, &configs);
        snapshot = snapshot.apply(&event, &decisions, &configs);
    }
    Ok(Some(snapshot))
}

/// One journal-driven sweep: reclaim leftover local dirs and prune ephemeral
/// refs of terminal blooms. The factored-out side, unit-testable against a
/// `SqliteStore`, a stub-runner [`LocalExecutor`], and a fake-GitHub-backed
/// shell without the mail harness.
pub(super) fn sweep(
    store: &mut dyn StoreBackend,
    local: Option<&LocalExecutor>,
    source: Option<&SourceShell>,
    policy: &SweepPolicy,
    now: SystemTime,
) -> rusqlite::Result<SweepReport> {
    let Some(snapshot) = replay_snapshot(store)? else {
        return Ok(SweepReport::default());
    };

    let outstanding = store.list_outstanding_nonces()?;
    let live: HashSet<&str> = outstanding.iter().map(String::as_str).collect();
    let live_blooms: HashSet<BloomId> =
        snapshot.blooms.iter().filter(|(_, record)| is_active_unlanded(record.status)).map(|(id, _)| *id).collect();
    let terminal: Vec<BloomId> = snapshot
        .blooms
        .iter()
        .filter(|(_, record)| matches!(record.status, BloomStatus::Landed | BloomStatus::Superseded))
        .map(|(id, _)| *id)
        .collect();

    let mut report = SweepReport::default();
    if let Some(local) = local {
        report.abandoned = local.reclaim_abandoned(&live);
        report.evidence = local.reclaim_consumed_evidence(&live, &live_blooms, policy.evidence_retain_days, now);
        if let Some(scratch) = policy.lane_scratch.as_deref() {
            report.scratch = local.reclaim_lane_scratch(scratch, &live);
        }
        report.targets = local.sweep_idle_targets(policy.lane_target_budget_bytes);
    }

    if let Some(source) = source {
        for bloom in &terminal {
            match source.prune_ephemeral_refs(bloom) {
                Ok(()) => report.refs += 1,
                Err(error) => tracing::warn!(
                    target: "aether_chassis_bloomery::janitor",
                    %error,
                    "janitor: ephemeral ref prune failed; will retry next tick",
                ),
            }
        }
    }

    Ok(report)
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
        let policy = SweepPolicy {
            lane_target_budget_bytes: config.lane_target_budget_bytes,
            evidence_retain_days: config.evidence_retain_days,
            lane_scratch: (!config.lane_scratch.is_empty()).then(|| PathBuf::from(config.lane_scratch)),
        };
        tracing::info!(
            target: "aether_chassis_bloomery::janitor",
            poll_interval_secs = config.poll_interval_secs,
            lane_target_budget_bytes = policy.lane_target_budget_bytes,
            evidence_retain_days = policy.evidence_retain_days,
            "janitor mounted; sweeping leftover worktrees, evidence, targets, and ephemeral refs",
        );
        Ok(JanitorReactorState {
            local: config.local,
            source: config.source,
            store: Some(store),
            policy,
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot tick so leftovers a prior crash left behind are
    /// reclaimed without waiting a full poll interval.
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
        match sweep(store, state.local.as_deref(), state.source.as_ref(), &state.policy, SystemTime::now()) {
            Ok(report) => {
                if report != SweepReport::default() {
                    tracing::info!(
                        target: "aether_chassis_bloomery::janitor",
                        abandoned = report.abandoned,
                        evidence = report.evidence,
                        scratch = report.scratch,
                        targets = report.targets,
                        refs = report.refs,
                        "janitor reclaimed leftover resources",
                    );
                }
            }
            Err(error) => tracing::warn!(
                target: "aether_chassis_bloomery::janitor",
                %error,
                "janitor sweep failed",
            ),
        }
    }
}
