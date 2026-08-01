//! The registry owner's drain accounting — ADR-0165's sharding denominator
//! (iamacoffeepot/aether#4176).
//!
//! Two rates, and the difference between them is the point.
//!
//! The **floor** comes from the sequential populate phase: `spawn_actor(..)
//! .finish()` blocks until each birth lands, so the queue holds one item and
//! every drain retires exactly one. A drainer that never batches cannot
//! amortize, so its rate is an unloaded service rate.
//!
//! The **ceiling** comes from [`fixture::CommitParent`] staging bursts: a
//! handler submits N births without waiting on any, so they queue together and
//! the drainer batches. That is the rate ADR-0165 means when it defers sharding
//! until churn exceeds ~5% of the measured ceiling.
//!
//! Reporting the floor as the ceiling is not a rounding error — the trigger
//! *divides* by it, so a floor makes the ratio look larger and biases toward
//! sharding earlier than the ADR intends. Both are reported, and
//! `queued_under_load` is the field that says which one you are reading.

use std::time::{Duration, Instant};

use aether_data::Kind;
use aether_substrate::{Registry, Subname};
use serde::{Deserialize, Serialize};

use super::fixture::{CloseBurst, CommitParent, CommitQuery, CommitReport, StageBurst};
use super::per_second;
use crate::SubstrateHarness;

/// Births staged per burst. Deep enough that the drainer has something to
/// amortize over (the measured `drain_max` tracks it closely), small enough
/// that one burst settles quickly.
const BURST: u32 = 256;

/// Bursts driven for the loaded sample. Several so the rate rests on more than
/// one drain cycle and a single unlucky burst cannot set the ceiling.
const BURSTS: u32 = 4;

/// One measurement of the owner's drain accounting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerCeiling {
    /// What drove this sample — `"sequential-spawn"` (the floor) or
    /// `"staged-burst"` (the ceiling). Named rather than inferred so a reader
    /// of the JSON never has to reconstruct which phase produced a rate.
    pub drive: String,
    pub commits_driven: u64,
    pub elapsed_nanos: u64,
    pub admitted: u64,
    pub drained: u64,
    pub drains: u64,
    /// Largest single drain. This is the amortization signal: 1 means the
    /// drainer retired items one at a time and the rate below is a floor.
    pub drain_max: u64,
    pub depth_max: u64,
    pub over_capacity: u64,
    pub shed: u64,
    pub busy_nanos: u64,
    /// `drained / busy_nanos` — items retired per second of actual draining.
    /// ADR-0165's denominator, but only when `queued_under_load` is true.
    pub items_per_sec_while_draining: Option<f64>,
    /// Whether the queue ever held more than one item. False means the drainer
    /// was never under pressure and the rate above is an unloaded service rate.
    pub queued_under_load: bool,
    /// `busy_nanos / elapsed` **over this benchmark's saturating drive**, not
    /// the sustained figure ADR-0165's 5% trigger reads.
    ///
    /// Both phases commit as fast as they can on purpose — that is how the
    /// rates get clean samples — so this says nothing about whether a real
    /// engine is near its sharding threshold. The trigger compares a
    /// *production* engine's duty cycle, read live from
    /// `Registry::owner_queue_metrics`, against the ceiling measured here.
    /// Reported only so each rate's provenance is legible.
    pub drive_duty_cycle: Option<f64>,
}

/// Read the owner's accounting after a phase, labelling it with what drove it.
///
/// The counters are cumulative over the process, so `before` is subtracted:
/// the loaded sample must not inherit the populate phase's drains, or the
/// ceiling would be diluted by the floor that preceded it.
pub fn sample(
    drive: &str,
    registry: &Registry,
    before: Option<&RawCounters>,
    commits: u64,
    elapsed: Duration,
) -> OwnerCeiling {
    let elapsed_nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    let Some(now) = RawCounters::read(registry) else {
        // Pre-seal, or an embedder that never attached an owner: no queue
        // exists, so there is nothing to report rather than a zero.
        return empty(drive, commits, elapsed_nanos);
    };
    let base = before.copied().unwrap_or_default();
    let drained = now.drained.saturating_sub(base.drained);
    let busy_nanos = now.busy_nanos.saturating_sub(base.busy_nanos);
    OwnerCeiling {
        drive: drive.to_owned(),
        commits_driven: commits,
        elapsed_nanos,
        admitted: now.admitted.saturating_sub(base.admitted),
        drained,
        drains: now.drains.saturating_sub(base.drains),
        // `drain_max` / `depth_max` are running maxima, not counters, so they
        // are reported as observed rather than differenced.
        drain_max: now.drain_max,
        depth_max: now.depth_max,
        over_capacity: now.over_capacity.saturating_sub(base.over_capacity),
        shed: now.shed.saturating_sub(base.shed),
        busy_nanos,
        items_per_sec_while_draining: per_second(drained, busy_nanos),
        queued_under_load: now.depth_max > 1,
        drive_duty_cycle: duty_cycle(busy_nanos, elapsed_nanos),
    }
}

/// Fraction of a phase's wall clock the owner spent draining. Differenced
/// counters, so this cannot reuse `RegistryQueueMetrics::duty_cycle`, which
/// reads the process-cumulative `busy_nanos`.
#[allow(
    clippy::cast_precision_loss,
    reason = "a duty cycle is a trend ratio; f64 is exact past any nanosecond count these phases accumulate"
)]
fn duty_cycle(busy_nanos: u64, elapsed_nanos: u64) -> Option<f64> {
    (elapsed_nanos > 0).then(|| busy_nanos as f64 / elapsed_nanos as f64)
}

/// The owner counters this module differences across a phase.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawCounters {
    pub admitted: u64,
    pub drained: u64,
    pub drains: u64,
    pub drain_max: u64,
    pub depth_max: u64,
    pub over_capacity: u64,
    pub shed: u64,
    pub busy_nanos: u64,
}

impl RawCounters {
    /// `None` before an owner is attached.
    #[must_use]
    pub fn read(registry: &Registry) -> Option<Self> {
        let metrics = registry.owner_queue_metrics()?;
        Some(Self {
            admitted: metrics.admitted,
            drained: metrics.drained,
            drains: metrics.drains,
            drain_max: metrics.drain_max,
            depth_max: metrics.depth_max,
            over_capacity: metrics.over_capacity,
            shed: metrics.shed,
            busy_nanos: metrics.busy_nanos,
        })
    }
}

fn empty(drive: &str, commits: u64, elapsed_nanos: u64) -> OwnerCeiling {
    OwnerCeiling {
        drive: drive.to_owned(),
        commits_driven: commits,
        elapsed_nanos,
        admitted: 0,
        drained: 0,
        drains: 0,
        drain_max: 0,
        depth_max: 0,
        over_capacity: 0,
        shed: 0,
        busy_nanos: 0,
        items_per_sec_while_draining: None,
        queued_under_load: false,
        drive_duty_cycle: None,
    }
}

/// Drive [`BURSTS`] staged bursts through `parent` and report the owner's
/// accounting over just that phase — the loaded ceiling.
///
/// Each burst is one settle-gated send, so it returns only once every staged
/// birth in it has completed and discharged its hold. That is also the
/// integrity check: a fixture that dropped a `TaskDone` would hang here rather
/// than quietly reporting a rate over fewer commits than it claims.
pub fn measure_loaded_ceiling(harness: &mut SubstrateHarness, parent: &str) -> OwnerCeiling {
    let before = RawCounters::read(harness.mail_registry());
    let start = Instant::now();
    let mut driven = 0_u64;
    for _ in 0..BURSTS {
        let payload = StageBurst { count: BURST }.encode_into_bytes();
        if let Err(error) = harness.send_bytes(parent, StageBurst::ID, payload) {
            tracing::warn!(target: "aether_perf", ?error, "staged burst did not settle");
            break;
        }
        driven += u64::from(BURST);
    }
    let elapsed = start.elapsed();
    let mut ceiling = sample("staged-burst", harness.mail_registry(), before.as_ref(), driven, elapsed);

    // Reconcile against the fixture's own tally. `commits_driven` is what the
    // benchmark asked for; this is what the parent actually staged and saw
    // completed, and a mismatch means the rate's denominator is wrong.
    if let Some(report) = query(harness, parent) {
        if report.staged != driven || report.succeeded + report.failed != report.staged {
            tracing::warn!(
                target: "aether_perf",
                requested = driven,
                staged = report.staged,
                succeeded = report.succeeded,
                failed = report.failed,
                "staged-burst tally disagrees with the drive; the loaded rate is over an uncertain commit count",
            );
        }
        ceiling.commits_driven = report.succeeded;
    }
    ceiling
}

/// Retire every child the staging parent still holds.
///
/// The ceiling phase leaves [`BURST`] × [`BURSTS`] extra routes behind, which
/// would otherwise inflate the table the read sweep walks well past the
/// `populated_mailboxes` the report states. Closing them keeps the reported
/// table size honest.
pub fn close_children(harness: &mut SubstrateHarness, parent: &str) {
    let payload = CloseBurst::default().encode_into_bytes();
    if let Err(error) = harness.send_bytes(parent, CloseBurst::ID, payload) {
        tracing::warn!(target: "aether_perf", ?error, "closing the staged children did not settle");
    }
}

/// Ask the staging parent for its birth tally.
pub fn query(harness: &mut SubstrateHarness, parent: &str) -> Option<CommitReport> {
    let request = CommitQuery::default().encode_into_bytes();
    match harness.send_bytes_and_await(parent, CommitQuery::ID, request) {
        Ok(reply) => CommitReport::decode_from_bytes(&reply),
        Err(error) => {
            tracing::warn!(target: "aether_perf", ?error, "commit-tally query failed");
            None
        }
    }
}

/// Spawn the staging parent and return its canonical address.
pub fn spawn_parent(harness: &SubstrateHarness) -> Option<String> {
    harness
        .spawn_actor::<CommitParent>(Subname::Named("commit"), (), ())
        .finish_with_name()
        .map_err(|error| tracing::warn!(target: "aether_perf", ?error, "staging parent spawn failed"))
        .ok()
        .map(|(_, name)| name)
}
