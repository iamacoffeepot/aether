//! `aether-perf-registry` (iamacoffeepot/aether#4176, #4274): drive the **real**
//! `Registry` K times and emit a banded `RegistryBandReport` as JSON on stdout.
//! Diagnostics go to stderr, so stdout stays pure JSON.
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
//!   with and without concurrent owner churn. Each contended cell reports the
//!   owner commits it observed, so the column cannot claim republication it did
//!   not drive.
//! - **The single-owner ceiling** — `drained / busy_nanos`, reported twice.
//!   The sequential populate phase yields a *floor* (blocking spawns never
//!   queue, so the drainer never batches); staged bursts yield the ceiling
//!   ADR-0165's "sustained churn exceeds approximately 5%" trigger divides by.
//!   The trigger has had its counter all along and never had this denominator.
//!
//! # Every number is replicated
//!
//! The read column's answer disagreed with an earlier measurement of the same
//! thing by ~28x at one reader (iamacoffeepot/aether#4274), and neither run
//! could be believed over the other because each was one draw. So the default
//! is not one sweep: it is K sweeps, each in a fresh process, reduced per cell
//! to a median and an IQR band (ADR-0085 §1–2). `-k 1` is available and honest
//! about what it is — a band over a single trial, which is a run without error
//! bars.
//!
//! ```text
//! aether-perf-registry                  # 9 trials, band on stdout
//! aether-perf-registry -k 21 --out band.json
//! ```
//!
//! Unlike `perf-trial` this reports throughput over a fixed window rather than
//! per-hop percentiles, so it carries no verdict and is not wired into the
//! merge gate. Run it when the registry's performance is the question.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::fs;
use std::process::ExitCode;

use aether_harness_substrate::perf::registry::band::{
    DEFAULT_TRIALS, OwnerBand, ReadBandCell, RegistryBandReport, run_banded_benchmark, selected_trial,
};
use aether_harness_substrate::perf::registry::run_registry_benchmark;
use aether_harness_substrate::perf::stats::BandStats;

struct Args {
    trials: usize,
    out: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut trials = DEFAULT_TRIALS;
    let mut out = None;

    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-k" | "--trials" => {
                let n = it.next().ok_or("-k needs a value")?;
                trials = n.parse().map_err(|_| format!("bad -k: {n}"))?;
            }
            "--out" => out = it.next(),
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    if trials == 0 {
        return Err("-k must be >= 1".to_owned());
    }
    Ok(Args { trials, out })
}

/// Render a throughput band as `median [p25..p75]` in millions per second.
fn mps(band: &BandStats) -> String {
    format!("{:>7.2} [{:>6.2}..{:>6.2}] M/s", band.median / 1e6, band.p25 / 1e6, band.p75 / 1e6)
}

/// Render a scaling band as `median [p25..p75]` with its position against flat.
///
/// The position is the whole point of replicating: `0.11x [0.11..0.12] below`
/// says every quartile of K trials put this reader count behind one reader,
/// which a bare `0.11x` from a single run cannot claim.
fn scaling(cell: &ReadBandCell) -> String {
    format!(
        "{:>5.2}x [{:>5.2}..{:>5.2}] {}",
        cell.scaling_vs_one.median,
        cell.scaling_vs_one.p25,
        cell.scaling_vs_one.p75,
        cell.scaling_position.label(),
    )
}

/// Print one owner phase's band, saying plainly whether its rate is the ceiling
/// or only a floor. `drain_max` is the tell: a band centred on 1 means the
/// drainer retired items one at a time and had nothing to amortize over.
fn report_owner(owner: &OwnerBand, trials: u64) {
    eprintln!(
        "owner [{}] {:>9.0} [{:>9.0}..{:>9.0}] items/s while draining   drain_max {:.0}",
        owner.drive,
        owner.items_per_sec_while_draining.median,
        owner.items_per_sec_while_draining.p25,
        owner.items_per_sec_while_draining.p75,
        owner.drain_max.median,
    );
    if owner.items_per_sec_while_draining.trials == 0 {
        eprintln!("      rate unavailable: no owner queue attached");
    } else if owner.queued_under_load_trials == trials {
        eprintln!("      measured under queue pressure in every trial (the ceiling)");
    } else if owner.queued_under_load_trials == 0 {
        eprintln!("      a FLOOR, not the ceiling: the queue never batched, so this rate cannot amortize.");
        eprintln!("      ADR-0165's trigger divides by the ceiling, so a floor biases toward early sharding.");
    } else {
        eprintln!(
            "      MIXED: only {}/{trials} trials queued under load, so this band pools a ceiling with a floor.",
            owner.queued_under_load_trials,
        );
    }
}

fn render(report: &RegistryBandReport) {
    eprintln!(
        "box: {} usable cores, {} chassis workers, {} mailboxes populated",
        report.usable_cores, report.chassis_workers, report.populated_mailboxes,
    );
    eprintln!(
        "{}/{} trials completed{}; band is median [p25..p75] across trials (ADR-0085)",
        report.trials_completed,
        report.trials_requested,
        if report.isolated {
            ", each a fresh process"
        } else {
            ", ALL IN ONE PROCESS — trials share process history and the band understates its own spread"
        },
    );

    for cell in &report.read_scaling {
        let note = if cell.oversubscribed {
            "  (oversubscribed)"
        } else {
            ""
        };
        // The commit band is the churn column's own evidence. Printed on every
        // row so a contended cell that drove nothing is visible here rather
        // than only in the JSON.
        eprintln!(
            "read  threads={:>2} churn={:<5} kinds={:<10} routes={:<11} commits={:>6.0}  {}  {}{note}",
            cell.threads,
            cell.owner_churn,
            cell.kind_mix.label(),
            cell.target_spread.label(),
            cell.owner_commits_observed.median,
            mps(&cell.lookups_per_sec),
            scaling(cell),
        );
    }

    report_owner(&report.owner_unloaded, report.trials_completed);
    match &report.owner_loaded {
        Some(loaded) => report_owner(loaded, report.trials_completed),
        None => eprintln!("owner loaded ceiling unavailable: the staging parent did not spawn"),
    }
}

/// Child mode: measure one trial and write its raw `RegistryReport` to stdout
/// for the parent to band. Silent on stderr beyond whatever the run itself
/// warns about — the parent renders the table, and K copies of it would bury
/// the one that matters.
fn run_one_trial() -> ExitCode {
    let Some(report) = run_registry_benchmark() else {
        eprintln!("perf-registry: no cells measured (no wgpu adapter, or the chassis failed to boot)");
        return ExitCode::from(2);
    };
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

fn main() -> ExitCode {
    if selected_trial().is_some() {
        return run_one_trial();
    }

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("perf-registry: {e}");
            return ExitCode::from(2);
        }
    };

    let report = run_banded_benchmark(args.trials);
    if report.trials_completed == 0 {
        eprintln!("perf-registry: no trials completed (no wgpu adapter, or the chassis failed to boot)");
        return ExitCode::from(2);
    }
    render(&report);

    let json = match serde_json::to_string(&report) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("perf-registry: serialize failed: {e}");
            return ExitCode::from(3);
        }
    };
    if let Some(path) = &args.out
        && let Err(e) = fs::write(path, &json)
    {
        eprintln!("perf-registry: write {path}: {e}");
        return ExitCode::from(1);
    }
    println!("{json}");
    ExitCode::SUCCESS
}
