//! `perf-trial` (iamacoffeepot/aether#1077): run one lifecycle latency
//! sweep and emit a [`TrialReport`] as JSON on stdout — the
//! fresh-process unit the `perf-compare` orchestrator runs K times per
//! side (ADR-0085 §1). Diagnostics go to stderr, so stdout stays pure
//! JSON.
//!
//! Config via env (so the orchestrator sets it once for both sides):
//!
//! - `AETHER_PERF_WORKERS` — comma list of pool sizes; the token `max`
//!   resolves to `available_parallelism() - 1`. Default `max`.
//! - `AETHER_PERF_TOPOS` — `ci` (depth-1/8 + fanout-4/8 + tree) or
//!   `full` (the whole default set). Default `ci`.
//! - `AETHER_PERF_FRAMES` — frames advanced per cell. Default `200`.
//! - `AETHER_LAT_PACE_HZ` — pace one frame per period (else flat-out).
//! - `AETHER_LAT_HEAVY_WORK` — when set, append CPU-heavy fan-outs
//!   (iamacoffeepot/aether#1074); unset, the topology set is unchanged.
//! - `AETHER_LAT_WIDE_FANOUT` — comma list of extra trivial fan-out
//!   widths to append (iamacoffeepot/aether#1075); unset, unchanged.
//! - `AETHER_PERF_GIT_SHA` — stamped into the report; falls back to
//!   `git rev-parse HEAD`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::process::{Command, ExitCode};

use aether_substrate_bundle::perf::harness::{
    SweepConfig, pace_hz_from_env, parse_topologies, parse_workers, run_sweep,
};
use aether_substrate_bundle::perf::report::TrialReport;

fn git_sha() -> Option<String> {
    if let Ok(s) = env::var("AETHER_PERF_GIT_SHA")
        && !s.is_empty()
    {
        return Some(s);
    }
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn main() -> ExitCode {
    let frames: u32 = env::var("AETHER_PERF_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let pace_hz = pace_hz_from_env();

    let cfg = SweepConfig {
        workers: parse_workers(),
        topologies: parse_topologies(),
        frames,
        pace_hz,
    };
    let cells = run_sweep(&cfg);
    if cells.is_empty() {
        eprintln!("perf-trial: no cells measured (no wgpu adapter, or every cell boot failed)");
        return ExitCode::from(2);
    }
    let report = TrialReport::from_cells(&cells, frames, pace_hz, git_sha());
    match serde_json::to_string(&report) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("perf-trial: serialize failed: {e}");
            ExitCode::from(3)
        }
    }
}
