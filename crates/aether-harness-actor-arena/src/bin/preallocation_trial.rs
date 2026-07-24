use std::io::{self, Write};

use aether_harness_actor_arena::{
    HolePattern, PreallocationConfig, PreallocationTarget, SweepMode, run_preallocation_trial,
};
use anyhow::Result;
use clap::Parser;

/// Run one arena-capacity estimate in a fresh process and emit JSON.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, value_enum, default_value = "native")]
    target: PreallocationTarget,

    #[arg(long, default_value_t = 65_536)]
    actors: usize,

    #[arg(long, default_value_t = 65_536)]
    capacity_hint: usize,

    #[arg(long, default_value_t = 16)]
    growth_pages: usize,

    #[arg(long, default_value_t = 64)]
    page_slots: usize,

    #[arg(long, default_value_t = 64)]
    state_bytes: usize,

    #[arg(long, default_value_t = 100)]
    live_percent: u8,

    #[arg(long, value_enum, default_value = "packed")]
    hole_pattern: HolePattern,

    #[arg(long, value_enum, default_value = "live-bitmap")]
    sweep_mode: SweepMode,

    #[arg(long, default_value_t = 80)]
    sweeps: usize,

    #[arg(long, default_value_t = 4_096)]
    burst_actors: usize,

    #[arg(long, default_value_t = 0x5eed_5eed_cafe_f00d)]
    seed: u64,

    /// Touch one byte per host page across reserved-but-unused state.
    #[arg(long)]
    touch_reserved: bool,

    /// Enable the perturbing global-allocation diagnostic pass for cold phases.
    #[arg(long)]
    instrument_allocations: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let report = run_preallocation_trial(PreallocationConfig {
        target: args.target,
        actors: args.actors,
        capacity_hint: args.capacity_hint,
        growth_pages: args.growth_pages,
        page_slots: args.page_slots,
        state_bytes: args.state_bytes,
        live_percent: args.live_percent,
        hole_pattern: args.hole_pattern,
        sweep_mode: args.sweep_mode,
        sweeps: args.sweeps,
        burst_actors: args.burst_actors,
        seed: args.seed,
        touch_reserved: args.touch_reserved,
        instrument_allocations: args.instrument_allocations,
    })?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &report)?;
    writeln!(output)?;
    Ok(())
}
