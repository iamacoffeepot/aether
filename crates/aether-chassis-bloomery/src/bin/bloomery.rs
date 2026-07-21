//! The `bloomery` binary: parse argv with [`BloomeryCli`], then boot
//! [`BloomeryChassis`] and run until a shutdown signal. Every knob resolves
//! argv > `AETHER_*` env > default (ADR-0090 unit d): `--rpc-port` shadows
//! `AETHER_RPC_PORT` (the engines cap injects it when it forks a bloomery),
//! `--store-path` shadows `AETHER_STORE_PATH` (the durable database file).

// `--describe` prints the binary manifest to stdout before the tracing
// subscriber is installed, then exits — the ADR-0115 provenance probe.
#![allow(clippy::print_stdout)]

use aether_chassis_bloomery::bloomery::{BloomeryChassis, BloomeryCli, BloomeryEnv, Chassis};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = BloomeryCli::parse();
    // `--describe` (ADR-0115): print this binary's manifest — chassis kind,
    // linked caps, build provenance — as JSON, then exit before boot.
    if cli.describe {
        println!("{}", serde_json::to_string(&BloomeryChassis::describe_manifest())?);
        return Ok(());
    }
    let env = BloomeryEnv::from_env_with_argv(&cli)?;
    // `build` installs the substrate tracing subscriber, so this logs.
    let chassis = BloomeryChassis::build(env)?;
    tracing::info!("aether-chassis-bloomery: bloomery chassis initialised (profile={})", BloomeryChassis::PROFILE);
    chassis.run()?;
    Ok(())
}
