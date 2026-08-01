//! Read scaling of the published route view (iamacoffeepot/aether#4176).
//!
//! `Registry::resolve_route_state` delegates to the same `route_lookup` the
//! mailer's route step runs, so this measures the production read rather than a
//! copy that could drift from it. Reader counts sweep past 8, where the
//! `spike/registry-view-contention` measurements put the effect.
//!
//! # The contended half, and why it reports its own churn
//!
//! The question ADR-0165 actually turns on is whether reads stay wait-free
//! *while the owner republishes* — a lock-free read should be indifferent to a
//! concurrent writer, and a reader-writer lock would not be.
//!
//! An earlier version of this sweep drove that column with
//! `Registry::lookup("…absent")`, on the reasoning that a miss is the class the
//! owner queue is bounded against. It is not: `lookup` resolves against the
//! published view and returns, so the "churn" thread was a second *reader* and
//! the column's ~11% cost at 8 readers was the price of one more reader, not of
//! republication. Measured directly, 200,000 absent lookups moved `admitted`,
//! `drains`, and `depth_max` by exactly zero.
//!
//! So churn is now real — [`super::fixture::CommitParent`] staging and closing
//! children, which commits through the owner — and, more importantly, each
//! contended cell **reports the owner commits observed during its own window**.
//! A column that counts its own churn cannot silently claim churn it did not
//! do, which is the failure this replaces.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_data::Kind;
use aether_substrate::mail::registry::RouteResolution;
use aether_substrate::{MailboxId, Registry};
use serde::{Deserialize, Serialize};

use super::fixture::{CloseBurst, StageBurst};
use super::owner::RawCounters;
use super::{CHASSIS_WORKERS, per_second, spawn_bench_thread, usable_cores};
use crate::SubstrateHarness;
use crate::perf::harness::Ping;

/// How long each read-scaling cell runs. Long enough to swamp thread start-up,
/// short enough that the whole sweep stays interactive.
const READ_WINDOW: Duration = Duration::from_millis(250);

/// Children staged per churn cycle. Small against the populated table so the
/// route count oscillates by a few percent rather than doubling mid-window —
/// the readers should be sweeping a table of roughly constant size while it is
/// being republished, not one that grows underneath them.
const CHURN_BURST: u32 = 16;

/// Reader counts swept. Past 8 because that is where the spike's contention
/// effect appeared, and the substrate's own dispatch harness tops out at 3
/// workers — one of the three mismatches that made the dispatch lane unable to
/// answer this question.
pub const READER_THREADS: &[usize] = &[1, 2, 4, 8, 16];

/// Read passes run before the first cell, on one thread, purely to warm the
/// path.
///
/// Every cell is reported relative to the single-thread cell, so a cold
/// baseline does not just mis-state one row — it rescales the whole column and
/// can turn a flat sweep into an apparently rising one. The first cell would
/// otherwise be the one that faults in the published view and the target list,
/// so the warm-up buys the baseline the same starting state its siblings get.
const WARMUP_PASSES: usize = 64;

/// One reader-count cell of the read-scaling sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadScalingCell {
    pub threads: u64,
    /// Whether the owner was concurrently republishing the table.
    pub owner_churn: bool,
    /// Owner items retired during this cell's window. **This is the column's
    /// own evidence**: a `owner_churn: true` cell with zero commits here did
    /// not measure what it claims, and an uncontended cell should read zero.
    pub owner_commits_observed: u64,
    pub lookups: u64,
    pub elapsed_nanos: u64,
    pub lookups_per_sec: f64,
    /// Throughput relative to this sweep's own single-thread cell at the same
    /// churn setting. Perfect scaling is `threads`.
    pub scaling_vs_one: Option<f64>,
    /// Readers plus chassis workers exceed the usable core count, so this cell
    /// measures the OS timeslicing threads rather than the registry's behaviour
    /// under load. Reported rather than dropped: the cliff is worth seeing, as
    /// long as nobody reads it as contention.
    pub oversubscribed: bool,
}

/// One read-scaling cell: `threads` readers hammering `resolve_route_state`
/// over `targets` for `READ_WINDOW`.
///
/// The churn runs on **this** thread rather than a spawned one. `SubstrateHarness`
/// is not `Sync` — it owns the chassis receivers — so the only thread that can
/// drive mail is the one holding it, and the window is spent driving churn
/// bursts instead of sleeping. That keeps the harness single-threaded without
/// needing to hand any part of it around.
pub fn read_cell(
    harness: &mut SubstrateHarness,
    targets: &[MailboxId],
    threads: usize,
    churn: Option<&str>,
    baseline: Option<f64>,
) -> ReadScalingCell {
    let registry = Arc::clone(harness.mail_registry());
    let kind = <Ping as Kind>::ID;
    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(AtomicU64::new(0));
    let before = RawCounters::read(&registry);

    let start = Instant::now();
    let readers: Vec<_> = (0..threads)
        .map(|worker| {
            let registry = Arc::clone(&registry);
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

    match churn {
        Some(parent) => drive_churn(harness, parent, READ_WINDOW),
        None => thread::sleep(READ_WINDOW),
    }
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        let _ = reader.join();
    }
    let elapsed = start.elapsed();

    let observed = RawCounters::read(&registry);
    let owner_commits_observed = match (before, observed) {
        (Some(before), Some(after)) => after.drained.saturating_sub(before.drained),
        _ => 0,
    };
    if churn.is_some() && owner_commits_observed == 0 {
        tracing::warn!(
            target: "aether_perf",
            threads,
            "a contended cell observed no owner commits; it is not measuring republication",
        );
    }

    let lookups = total.load(Ordering::Relaxed);
    let elapsed_nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    let lookups_per_sec = per_second(lookups, elapsed_nanos).unwrap_or(0.0);
    ReadScalingCell {
        threads: threads as u64,
        owner_churn: churn.is_some(),
        owner_commits_observed,
        lookups,
        elapsed_nanos,
        lookups_per_sec,
        scaling_vs_one: baseline.filter(|one| *one > 0.0).map(|one| lookups_per_sec / one),
        oversubscribed: threads + CHASSIS_WORKERS > usable_cores(),
    }
}

/// Walk the read path on this thread before the sweep starts, so the
/// single-thread baseline every other cell is scaled against is measured warm.
/// See `WARMUP_PASSES` for why a cold baseline is worse than a slow one.
pub fn warm_read_path(registry: &Registry, targets: &[MailboxId]) {
    let kind = <Ping as Kind>::ID;
    for _ in 0..WARMUP_PASSES {
        for &target in targets {
            let _ = registry.resolve_route_state(kind, target);
        }
    }
}

/// Republish the route table for `window` by staging and then closing a small
/// burst of children through `parent`, which commits both ways through the
/// owner. Stage-then-close keeps the table's size oscillating by
/// `CHURN_BURST` rather than growing for the whole window.
///
/// Each send is settle-gated, so a cycle returns only once its births have
/// landed; the loop re-checks the deadline between cycles rather than
/// interrupting one, so it overruns by at most one cycle.
fn drive_churn(harness: &mut SubstrateHarness, parent: &str, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        let stage = StageBurst { count: CHURN_BURST }.encode_into_bytes();
        if let Err(error) = harness.send_bytes(parent, StageBurst::ID, stage) {
            tracing::warn!(target: "aether_perf", ?error, "churn stage did not settle; ending churn early");
            return;
        }
        let close = CloseBurst::default().encode_into_bytes();
        if let Err(error) = harness.send_bytes(parent, CloseBurst::ID, close) {
            tracing::warn!(target: "aether_perf", ?error, "churn close did not settle; ending churn early");
            return;
        }
    }
}
