//! `aether-perf-registry` (iamacoffeepot/aether#4176): drive the **real**
//! `Registry` and emit a `RegistryReport` as JSON on stdout. Diagnostics go
//! to stderr, so stdout stays pure JSON.
//!
//! ADR-0165 names the registry-view contention spike as its performance
//! baseline. That spike is a self-contained nested workspace over toy tables
//! with no dependency on `aether-substrate`: it priced approaches against each
//! other and chose the double buffer, which was the right instrument for that
//! job, but re-running it measures the same toy tables it always did. The CI
//! dispatch lane cannot stand in either — it measures dispatch latency rather
//! than reader-contention scaling, tops out at 3 workers where the effect
//! appears past 8, and is informational by construction.
//!
//! Two numbers come out, matching the two claims the ADR leans on:
//!
//! - **Read scaling** — `resolve_route_state` throughput against reader count,
//!   with and without concurrent owner churn. Reads never touch
//!   `Registry::inner`, so the expectation is near-linear scaling and
//!   indifference to the churn column; a contended-column collapse would mean
//!   the lock-free claim does not hold in practice.
//! - **The single-owner ceiling** — `drained / busy_nanos`, plus the duty cycle
//!   ADR-0165's "sustained churn exceeds approximately 5%" trigger reads
//!   against. The trigger has a counter but never had this denominator, so it
//!   could not fire.
//!
//! Unlike `perf-trial` this reports throughput over a fixed window rather than
//! per-hop percentiles, so it carries no verdict and is not wired into the
//! merge gate. Run it when the registry's performance is the question.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use aether_harness_substrate::perf::registry::run_registry_benchmark;

fn main() -> ExitCode {
    let Some(report) = run_registry_benchmark() else {
        eprintln!("perf-registry: no cells measured (no wgpu adapter, or the chassis failed to boot)");
        return ExitCode::from(2);
    };

    eprintln!(
        "box: {} usable cores, {} chassis workers, {} mailboxes populated",
        report.usable_cores, report.chassis_workers, report.populated_mailboxes,
    );
    for cell in &report.read_scaling {
        let scaling = cell.scaling_vs_one.map_or_else(|| "-".to_owned(), |s| format!("{s:.2}x"));
        let note = if cell.oversubscribed {
            "  (oversubscribed)"
        } else {
            ""
        };
        eprintln!(
            "read  threads={:>2} churn={:<5} {:>12.0} lookups/s  {scaling}{note}",
            cell.threads, cell.owner_churn, cell.lookups_per_sec,
        );
    }
    let owner = &report.owner;
    eprintln!(
        "owner commits={} drained={} drains={} drain_max={} depth_max={} over_capacity={} shed={}",
        owner.commits_driven,
        owner.drained,
        owner.drains,
        owner.drain_max,
        owner.depth_max,
        owner.over_capacity,
        owner.shed,
    );
    match owner.items_per_sec_while_draining {
        Some(rate) if owner.queued_under_load => {
            eprintln!("owner {rate:.0} items/s while draining, measured under queue pressure");
        }
        Some(rate) => {
            eprintln!("owner {rate:.0} items/s while draining — a FLOOR, not the ceiling:");
            eprintln!("      depth_max=1, so the queue never batched and this cannot amortize.");
            eprintln!("      ADR-0165's trigger divides by the ceiling, so a floor biases toward early sharding.");
        }
        None => eprintln!("owner rate unavailable: no owner queue attached"),
    }

    match serde_json::to_string(&report) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("perf-registry: serialize failed: {e}");
            ExitCode::from(3)
        }
    }
}
