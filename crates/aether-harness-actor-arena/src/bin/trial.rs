use std::io::{self, Write};

use aether_harness_actor_arena::{AccessPattern, Backend, TrialConfig, Workload, run_trial};
use anyhow::Result;
use clap::Parser;

/// Run one actor-storage experiment in a fresh process and emit JSON.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, value_enum)]
    backend: Backend,

    #[arg(long, value_enum, default_value = "dispatch")]
    workload: Workload,

    #[arg(long, default_value_t = 1_024)]
    actors: usize,

    #[arg(long, default_value_t = 1_000_000)]
    mails: u64,

    #[arg(long, default_value_t = 16)]
    mails_per_activation: usize,

    #[arg(long, default_value_t = 64)]
    page_slots: usize,

    #[arg(long, default_value_t = 256)]
    state_bytes: usize,

    #[arg(long, value_enum, default_value = "random")]
    pattern: AccessPattern,

    #[arg(long, default_value_t = 0x5eed_5eed_cafe_f00d)]
    seed: u64,

    #[arg(long, default_value_t = 100_000)]
    warmup_mails: u64,

    /// Enable the perturbing global-allocation diagnostic pass.
    #[arg(long)]
    instrument_allocations: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let report = run_trial(TrialConfig {
        backend: args.backend,
        workload: args.workload,
        actors: args.actors,
        mails: args.mails,
        mails_per_activation: args.mails_per_activation,
        page_slots: args.page_slots,
        state_bytes: args.state_bytes,
        pattern: args.pattern,
        seed: args.seed,
        warmup_mails: args.warmup_mails,
        instrument_allocations: args.instrument_allocations,
    })?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &report)?;
    writeln!(output)?;
    Ok(())
}
