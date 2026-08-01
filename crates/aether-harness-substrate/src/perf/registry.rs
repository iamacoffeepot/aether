//! The real-`Registry` benchmark (iamacoffeepot/aether#4176).
//!
//! ADR-0165 names "the existing contention benchmark" as the baseline for the
//! view and owner conversion, but that benchmark is a self-contained spike over
//! toy tables — it never depended on `aether-substrate` and cannot regress.
//! This drives the **real** `Registry` inside a booted chassis instead, and
//! answers the two questions the ADR leans on:
//!
//! - **Read scaling.** Does the lock-free published view actually scale with
//!   reader count? [`Registry::resolve_route_state`] delegates to the same
//!   `route_lookup` the mailer's route step runs, so this measures the
//!   production read rather than a copy of it. Swept past 8 threads (where the
//!   spike's effect appeared) with and without concurrent owner churn.
//! - **The single-owner ceiling.** ADR-0165 defers sharding until sustained
//!   churn exceeds roughly 5% of the measured ceiling. `RegistryQueueMetrics`
//!   carries both terms — the ceiling is `drained` over `busy_nanos`, the duty
//!   cycle is `busy_nanos` over wall time — so the trigger only ever needed
//!   something to drive it.
//!
//! Both are throughput measures over a fixed window, not per-hop percentiles,
//! so they do not share `perf::harness`'s trace-ring harvest.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_data::Kind;
use aether_substrate::mail::registry::RouteResolution;
use aether_substrate::{MailboxId, Registry, Subname};
use serde::{Deserialize, Serialize};

use crate::SubstrateHarness;
use crate::perf::harness::{Ping, Relay, RelayConfig, relay_id};

/// Usable cores, for reading the sweep. A cell whose readers plus the chassis's
/// own workers exceed this is oversubscribed, and its throughput drop is the
/// scheduler timeslicing rather than anything about the registry.
fn usable_cores() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// Workers the read sweep's chassis runs, counted against [`usable_cores`]
/// when deciding whether a reader count oversubscribes the box.
const CHASSIS_WORKERS: usize = 2;

/// Mailboxes populated into the table before the read sweep, and the owner
/// commits that drive the ceiling sample. Large enough that the ceiling rests
/// on hundreds of drained items rather than a handful, small enough that the
/// populate phase stays well under a second.
const POPULATED_MAILBOXES: usize = 256;

/// How long each read-scaling cell runs. Long enough to swamp thread start-up,
/// short enough that the whole sweep stays interactive.
const READ_WINDOW: Duration = Duration::from_millis(250);

/// Reader counts swept. Past 8 because that is where the spike's contention
/// effect appeared, and the substrate's own harness tops out at 3 workers —
/// one of the three mismatches that made the dispatch lane unable to answer
/// this question.
const READER_THREADS: &[usize] = &[1, 2, 4, 8, 16];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryReport {
    pub schema: String,
    pub populated_mailboxes: u64,
    /// Usable cores on the measuring box. Every scaling figure has to be read
    /// against this — a cell past it is timeslicing, not contending.
    pub usable_cores: u64,
    pub chassis_workers: u64,
    pub read_scaling: Vec<ReadScalingCell>,
    pub owner: OwnerCeiling,
}

/// One reader-count cell of the read-scaling sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadScalingCell {
    pub threads: u64,
    /// Whether a writer was concurrently churning the table through the owner.
    /// The contended half of the question: a lock-free read should be
    /// indifferent to it.
    pub owner_churn: bool,
    pub lookups: u64,
    pub elapsed_nanos: u64,
    pub lookups_per_sec: f64,
    /// Throughput relative to this sweep's own single-thread cell at the same
    /// churn setting. Perfect scaling is `threads`.
    pub scaling_vs_one: Option<f64>,
    /// Readers plus chassis workers exceed the usable core count, so this cell
    /// measures the OS timeslicing threads rather than the registry's
    /// behaviour under load. Reported rather than dropped: the cliff is worth
    /// seeing, as long as nobody reads it as contention.
    pub oversubscribed: bool,
}

/// The owner's drain accounting over the populate phase.
///
/// **This is a floor on the ceiling, not the ceiling**, and `queued_under_load`
/// is how you tell. The populate phase spawns sequentially, and each
/// `spawn_actor(..).finish()` blocks until its commit lands, so the queue never
/// holds more than one item and every drain retires exactly one. A drainer that
/// never batches cannot amortize, so the rate below understates what a queued
/// owner sustains.
///
/// The direction of that error matters: ADR-0165's trigger fires when duty
/// cycle exceeds ~5% *of the ceiling*, so publishing a floor as the ceiling
/// biases toward sharding earlier than the ADR intends. Measuring the loaded
/// ceiling needs concurrent commits, which means a stager that discharges its
/// ADR-0093 `TaskDone` — see the issue thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerCeiling {
    pub commits_driven: u64,
    pub elapsed_nanos: u64,
    pub admitted: u64,
    pub drained: u64,
    pub drains: u64,
    pub drain_max: u64,
    pub depth_max: u64,
    pub over_capacity: u64,
    pub shed: u64,
    pub busy_nanos: u64,
    /// `drained / busy_nanos` — items retired per second of actual draining.
    /// A lower bound on ADR-0165's denominator while `queued_under_load` is
    /// false.
    pub items_per_sec_while_draining: Option<f64>,
    /// Whether the queue ever held more than one item, i.e. whether the drainer
    /// was under any pressure at all. False means the rate above is an unloaded
    /// service rate.
    pub queued_under_load: bool,
    /// `busy_nanos / elapsed` **over this benchmark's saturating drive**, not
    /// the sustained figure ADR-0165's 5% trigger reads.
    ///
    /// The populate phase commits as fast as it can on purpose — that is how
    /// the ceiling above gets a clean sample — so this lands near 1.0 by
    /// construction and says nothing about whether a real engine is near its
    /// sharding threshold. The trigger compares a *production* engine's duty
    /// cycle, read live from `Registry::owner_queue_metrics`, against the
    /// ceiling this benchmark supplies. Reported here only so the ceiling's
    /// provenance is legible.
    pub drive_duty_cycle: Option<f64>,
}

/// Boot a chassis, populate the route table with real spawned actors, then
/// measure. Returns `None` on a box with no wgpu adapter, matching how
/// `perf::harness::run_sweep_samples` skips.
#[must_use]
pub fn run_registry_benchmark() -> Option<RegistryReport> {
    let harness = SubstrateHarness::builder().with_workers(Some(CHASSIS_WORKERS)).size(16, 16).build().ok()?;

    // Populate through the real spawn path so the table holds the same shape a
    // running engine's does — live routes with published endpoints, reached the
    // way the owner publishes them.
    let spawn_start = Instant::now();
    let mut spawned = 0_u64;
    for i in 0..POPULATED_MAILBOXES {
        let config = RelayConfig { downstreams: Arc::from(Vec::new()), work_iters: 0 };
        if harness.spawn_actor::<Relay>(Subname::Named(&i.to_string()), config, ()).finish().is_err() {
            break;
        }
        spawned += 1;
    }
    let spawn_elapsed = spawn_start.elapsed();
    if spawned == 0 {
        return None;
    }

    let owner = owner_service_rate(harness.mail_registry(), spawned, spawn_elapsed);
    let registry = Arc::clone(harness.mail_registry());
    let targets: Vec<MailboxId> = (0..usize::try_from(spawned).unwrap_or(usize::MAX)).map(relay_id).collect();

    let mut read_scaling = Vec::new();
    for &churn in &[false, true] {
        let mut baseline = None;
        for &threads in READER_THREADS {
            let cell = read_cell(&registry, &targets, threads, churn, baseline);
            baseline.get_or_insert(cell.lookups_per_sec);
            read_scaling.push(cell);
        }
    }

    Some(RegistryReport {
        schema: "aether.perf.registry.v1".to_owned(),
        populated_mailboxes: spawned,
        usable_cores: usable_cores() as u64,
        chassis_workers: CHASSIS_WORKERS as u64,
        read_scaling,
        owner,
    })
}

/// Read the owner's drain accounting after `spawns` sequential registrations.
///
/// Each commit is real registry work, so the per-item rate is meaningful — but
/// sequential blocking spawns never queue, so it is an *unloaded* rate. See
/// [`OwnerCeiling`] for why that distinction is load-bearing rather than
/// pedantic.
fn owner_service_rate(registry: &Arc<Registry>, spawns: u64, elapsed: Duration) -> OwnerCeiling {
    let elapsed_nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    let Some(metrics) = registry.owner_queue_metrics() else {
        // Pre-seal, or an embedder that never attached an owner: no queue
        // exists, so there is nothing to report rather than a zero.
        return empty_ceiling(spawns, elapsed_nanos);
    };
    OwnerCeiling {
        commits_driven: spawns,
        elapsed_nanos,
        admitted: metrics.admitted,
        drained: metrics.drained,
        drains: metrics.drains,
        drain_max: metrics.drain_max,
        depth_max: metrics.depth_max,
        over_capacity: metrics.over_capacity,
        shed: metrics.shed,
        busy_nanos: metrics.busy_nanos,
        items_per_sec_while_draining: per_second(metrics.drained, metrics.busy_nanos),
        queued_under_load: metrics.depth_max > 1,
        drive_duty_cycle: metrics.duty_cycle(elapsed),
    }
}

/// A ceiling report for a phase that could not run — no owner queue attached,
/// or a chassis that failed to boot.
fn empty_ceiling(commits: u64, elapsed_nanos: u64) -> OwnerCeiling {
    OwnerCeiling {
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

/// One read-scaling cell: `threads` readers hammering `resolve_route_state`
/// over `targets` for [`READ_WINDOW`], optionally against a concurrent writer.
fn read_cell(
    registry: &Arc<Registry>,
    targets: &[MailboxId],
    threads: usize,
    owner_churn: bool,
    baseline: Option<f64>,
) -> ReadScalingCell {
    let kind = <Ping as Kind>::ID;
    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(AtomicU64::new(0));

    let churner = owner_churn.then(|| spawn_churn(Arc::clone(registry), Arc::clone(&stop)));

    let start = Instant::now();
    let readers: Vec<_> = (0..threads)
        .map(|worker| {
            let registry = Arc::clone(registry);
            let stop = Arc::clone(&stop);
            let total = Arc::clone(&total);
            // Each reader starts at a different offset so they are not walking
            // the id list in lockstep, which would read as better locality than
            // a real dispatch mix has.
            let targets: Vec<MailboxId> = targets.iter().copied().cycle().skip(worker).take(targets.len()).collect();
            spawn_bench_thread(move || {
                let mut count = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    for &target in &targets {
                        // Consume the result so the read cannot be elided.
                        if registry.resolve_route_state(kind, target) == RouteResolution::Live {
                            count += 1;
                        }
                    }
                }
                total.fetch_add(count, Ordering::Relaxed);
            })
        })
        .collect();

    thread::sleep(READ_WINDOW);
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        let _ = reader.join();
    }
    let elapsed = start.elapsed();
    if let Some(churner) = churner {
        let _ = churner.join();
    }

    let lookups = total.load(Ordering::Relaxed);
    let elapsed_nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    let lookups_per_sec = per_second(lookups, elapsed_nanos).unwrap_or(0.0);
    ReadScalingCell {
        threads: threads as u64,
        owner_churn,
        lookups,
        elapsed_nanos,
        lookups_per_sec,
        scaling_vs_one: baseline.filter(|one| *one > 0.0).map(|one| lookups_per_sec / one),
        oversubscribed: threads + CHASSIS_WORKERS > usable_cores(),
    }
}

/// A writer that keeps the published route view moving while readers run, so
/// the contended cells measure readers against real republication rather than
/// a frozen snapshot.
fn spawn_churn(registry: Arc<Registry>, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    spawn_bench_thread(move || {
        while !stop.load(Ordering::Relaxed) {
            // A miss walks the same published view a hit does and is the
            // sheddable class the owner queue is bounded against, so it churns
            // the read side without registering state the sweep must undo.
            let _ = registry.lookup("aether.perf.registry.absent");
        }
    })
}

#[allow(
    clippy::disallowed_methods,
    reason = "benchmark infrastructure below the actor/mail layer: these threads send no mail and take no settlement hold, so the ADR-0080 §12 umbrella has nothing to carry"
)]
fn spawn_bench_thread(body: impl FnOnce() + Send + 'static) -> thread::JoinHandle<()> {
    thread::spawn(body)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "throughput is a trend ratio; f64 is exact past any count these windows accumulate"
)]
fn per_second(count: u64, nanos: u64) -> Option<f64> {
    (nanos > 0).then(|| count as f64 * 1_000_000_000.0 / nanos as f64)
}
