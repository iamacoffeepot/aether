//! Lifecycle latency sweep engine (iamacoffeepot/aether#1057, #1077).
//!
//! The reusable core of the latency harness, lifted out of the
//! `#[cfg(test)]` `mail_latency` module so the `perf-trial` binary can
//! drive it (iamacoffeepot/aether#1077). [`run_sweep`] wires synthetic
//! relay actors into a topology, drives the substrate's real lifecycle
//! (`advance` → `Tick` fan-out → a tick-reactive source → the relay
//! chain), harvests the resident trace ring (ADR-0080) once per cell,
//! and returns per-cell [`CellResult`] percentiles. It performs no I/O
//! itself — callers render (the harness test prints a table; the
//! `perf-trial` bin emits JSON).
//!
//! **Worker count is the dominant variable.** Post issue #635 actors
//! are `Pooled` by default — they share a worker pool, not one thread
//! each. A depth chain with one root in flight serialises regardless of
//! pool size; a fan-out either parallelises across workers or queues on
//! a small pool. So the sweep takes the worker set as an axis.

// Dev/harness tooling: every `*_from_env` knob in this latency-sweep harness reads
// its run parameters from env (workers / topology / pacing / tiers / fan-out).
// This is a harness driver, not a capability — there is no config layer in scope,
// so the whole module opts out of the env-read ban.
#![allow(clippy::disallowed_methods)]

mod cell;
mod keepup;
mod kinds;
mod knobs;
mod percentiles;
mod relay;
mod sweep;
mod throughput;
mod tick;
mod topology;

pub use cell::{CellResult, CellSamples, run_cell};
pub use keepup::KeepUp;
pub use kinds::{CountQuery, CountReport, Ping};
pub use knobs::{
    DEFAULT_HEAVY_WORK_ITERS, DEFAULT_REAL_PACE_HZ, DEFAULT_SATURATE_BACKLOG, SCHEDULER_TUNING_ENV_KEYS,
    drive_for_tier, drive_from_env, effective_trace_ring_cap, heavy_work_iters_from_env, pace_hz_from_env,
    parse_workers, saturate_backlog_from_env, scheduler_tuning_from_env, tiers_from_env, wide_fanout_widths_from_env,
};
pub use percentiles::{Stats, TAIL_MASS_MULTIPLE, summarize};
pub use relay::{Relay, RelayConfig, relay_id};
pub use sweep::{Drive, SweepConfig, run_sweep, run_sweep_samples};
pub use tick::{TickSource, ticksrc_id};
pub use topology::{
    REAL_CODEC_WORK_ITERS, REAL_FANOUT_N, REAL_LOGIC_WORK_ITERS, REAL_UI_FOLLOWUP_STEPS, Tier, Topology,
    default_topologies, depth_chain, fanout, fanout_heavy, max_out_degree, parse_topologies, socket_server,
    tick_broadcast, two_level_tree, two_level_tree_heavy, two_level_tree_router_heavy, ui_roundtrip,
};
