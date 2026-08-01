//! The real-`Registry` benchmark (iamacoffeepot/aether#4176).
//!
//! ADR-0165 names "the existing contention benchmark" as the baseline for the
//! view and owner conversion, but that benchmark is a self-contained spike over
//! toy tables — it never depended on `aether-substrate` and cannot regress.
//! This drives the **real** `Registry` inside a booted chassis instead, and
//! answers the two questions the ADR leans on:
//!
//! - **Read scaling** ([`read`]) — does the lock-free published view scale with
//!   reader count, and does it stay indifferent to a concurrently republishing
//!   owner? Swept past 8 threads, where the spike's effect appeared.
//! - **The single-owner ceiling** ([`owner`]) — ADR-0165 defers sharding until
//!   sustained churn exceeds roughly 5% of the measured ceiling.
//!   `RegistryQueueMetrics` carries both terms; what was missing was something
//!   to drive the denominator hard enough to batch.
//!
//! [`fixture`] is what drives both: a staging parent that submits N births in
//! one handler pass, which is the only way to make the owner queue deep and is
//! how production reaches the owner anyway.
//!
//! Both are throughput measures over a fixed window, not per-hop percentiles,
//! so they do not share `perf::harness`'s trace-ring harvest.

pub mod fixture;
pub mod owner;
pub mod read;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use aether_substrate::{MailboxId, Subname};
use serde::{Deserialize, Serialize};

use crate::SubstrateHarness;
use crate::perf::harness::{Relay, RelayConfig, relay_id};
use owner::OwnerCeiling;
use read::ReadScalingCell;

/// Usable cores, for reading the sweep. A cell whose readers plus the chassis's
/// own workers exceed this is oversubscribed, and its throughput drop is the
/// scheduler timeslicing rather than anything about the registry.
fn usable_cores() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// Workers the read sweep's chassis runs, counted against [`usable_cores`]
/// when deciding whether a reader count oversubscribes the box.
const CHASSIS_WORKERS: usize = 2;

/// Mailboxes populated into the table before the read sweep. Large enough that
/// the sweep reads a table of production shape rather than a handful of rows,
/// small enough that the populate phase stays well under a second.
const POPULATED_MAILBOXES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryReport {
    pub schema: String,
    pub populated_mailboxes: u64,
    /// Usable cores on the measuring box. Every scaling figure has to be read
    /// against this — a cell past it is timeslicing, not contending.
    pub usable_cores: u64,
    pub chassis_workers: u64,
    pub read_scaling: Vec<ReadScalingCell>,
    /// The unloaded service rate, from the sequential populate phase. A
    /// **floor** on ADR-0165's denominator: blocking spawns never queue, so the
    /// drainer never batches.
    pub owner_unloaded: OwnerCeiling,
    /// The loaded ceiling, from staged bursts — ADR-0165's actual denominator.
    /// `None` if the staging parent could not be spawned.
    pub owner_loaded: Option<OwnerCeiling>,
}

/// Boot a chassis, populate the route table with real spawned actors, then
/// measure. Returns `None` on a box where the chassis will not boot, matching
/// how `perf::harness::run_cell` skips.
#[must_use]
pub fn run_registry_benchmark() -> Option<RegistryReport> {
    let mut harness = SubstrateHarness::builder().with_workers(Some(CHASSIS_WORKERS)).size(16, 16).build().ok()?;

    // Populate through the real spawn path so the table holds the same shape a
    // running engine's does — live routes with published endpoints, reached the
    // way the owner publishes them. Sequential and blocking, which is exactly
    // why the rate it yields is a floor rather than the ceiling.
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
    let owner_unloaded = owner::sample("sequential-spawn", harness.mail_registry(), None, spawned, spawn_elapsed);

    // The staging parent serves both remaining phases: it drives the loaded
    // ceiling, and it is the writer the contended read cells churn against.
    let parent = owner::spawn_parent(&harness);
    let owner_loaded = parent.as_deref().map(|parent| {
        let ceiling = owner::measure_loaded_ceiling(&mut harness, parent);
        // Retire the burst's children before the read sweep, so the table the
        // readers walk is the size this report claims.
        owner::close_children(&mut harness, parent);
        ceiling
    });

    let targets: Vec<MailboxId> = (0..usize::try_from(spawned).unwrap_or(usize::MAX)).map(relay_id).collect();
    read::warm_read_path(harness.mail_registry(), &targets);

    let mut read_scaling = Vec::new();
    for churn in [None, parent.as_deref()] {
        let mut baseline = None;
        for &threads in read::READER_THREADS {
            let cell = read::read_cell(&mut harness, &targets, threads, churn, baseline);
            baseline.get_or_insert(cell.lookups_per_sec);
            read_scaling.push(cell);
        }
    }

    Some(RegistryReport {
        schema: "aether.perf.registry.v2".to_owned(),
        populated_mailboxes: spawned,
        usable_cores: usable_cores() as u64,
        chassis_workers: CHASSIS_WORKERS as u64,
        read_scaling,
        owner_unloaded,
        owner_loaded,
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
