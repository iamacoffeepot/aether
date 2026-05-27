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
//! - `AETHER_PERF_TIER` — comma list of workload tiers to sweep: `light`,
//!   `heavy`, `real` (e.g. `light,heavy`). Default `light` (ADR-0085
//!   amendment). The *tier* axis — which classes of shape run, and which
//!   carry a verdict (only `light`; heavy / real are characterisation).
//!   The `real` tier (`socket-server` / `tick-broadcast` / `ui-roundtrip`) is
//!   always driven **paced** — `AETHER_LATENCY_PACE_HZ` or a 60 Hz default —
//!   regardless of `AETHER_PERF_DRIVE`, modelling a client/server round-trip
//!   rather than a flood; light / heavy keep the `AETHER_PERF_DRIVE` mode.
//! - `AETHER_PERF_TOPOS` — the breadth knob *within* a tier: `ci`
//!   (depth-1/8 + fanout-4/8 + tree) or `full` (the whole default set).
//!   Default `ci`. Orthogonal to `AETHER_PERF_TIER`.
//! - `AETHER_PERF_FRAMES` — frames advanced per cell. Default `200`.
//! - `AETHER_PERF_DRIVE` — `latency` (per-hop spans; default) or
//!   `saturate` (completed mails/sec under a backlog flood,
//!   iamacoffeepot/aether#1202).
//! - `AETHER_PERF_BACKLOG` — per-tick `Ping` burst in `saturate` mode
//!   (default `512`, clamped to the trace ring capacity).
//! - `AETHER_LATENCY_PACE_HZ` — `latency` mode only: pace one frame per
//!   period (else flat-out).
//! - `AETHER_LATENCY_HEAVY_WORK` — the heavy-tier per-node `busy_spin`
//!   *magnitude* (iamacoffeepot/aether#1074). Now magnitude-only: the tier
//!   selector (`AETHER_PERF_TIER`) gates *whether* heavy shapes run, so
//!   this just sizes the CPU burn, defaulting to a non-zero count when the
//!   heavy tier is active and the var is unset. Calibrate as before — set a
//!   count, read the per-leaf µs off the HANDLER DUR column, adjust.
//! - `AETHER_LATENCY_WIDE_FANOUT` — comma list of extra trivial (light)
//!   fan-out widths to append (iamacoffeepot/aether#1075); unset, unchanged.
//! - `AETHER_PERF_GIT_SHA` — stamped into the report; falls back to
//!   `git rev-parse HEAD`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::process::{Command, ExitCode};

use aether_substrate_bundle::perf::harness::{
    Drive, SweepConfig, drive_from_env, parse_topologies, parse_workers, run_sweep,
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
    let drive = drive_from_env();

    let cfg = SweepConfig {
        workers: parse_workers(),
        topologies: parse_topologies(),
        frames,
        drive,
    };
    let cells = run_sweep(&cfg);
    if cells.is_empty() {
        eprintln!("perf-trial: no cells measured (no wgpu adapter, or every cell boot failed)");
        return ExitCode::from(2);
    }
    // A `Saturate` run emits the throughput section only (latency under
    // saturation is contended noise); a `Latency` run emits the latency
    // section as before (iamacoffeepot/aether#1202).
    let report = match drive {
        Drive::Saturate { .. } => TrialReport::from_throughput_cells(&cells, frames, git_sha()),
        Drive::Latency { pace_hz } => TrialReport::from_cells(&cells, frames, pace_hz, git_sha()),
    };
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
