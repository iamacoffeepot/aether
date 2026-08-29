//! The `bloomery` binary: parse argv with [`BloomeryCli`], then boot
//! [`BloomeryChassis`] and run until a shutdown signal. Every knob resolves
//! argv > `AETHER_*` env > default (ADR-0090 unit d): `--rpc-port` shadows
//! `AETHER_RPC_PORT` (the engines cap injects it when it forks a bloomery),
//! `--store-path` shadows `AETHER_STORE_PATH` (the durable database file).

use std::io::{self, Write as _};
use std::process;

use aether_chassis_bloomery::bloomery::{BloomeryChassis, BloomeryCli, BloomeryEnv, Chassis, KitReport};
use aether_chassis_bloomery::store::check_store;
use aether_substrate::chassis::{PreludeFlags, run_chassis_prelude};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = BloomeryCli::parse();
    // ADR-0162 shared prelude: `--describe` (ADR-0115 manifest) prints and exits
    // before Init; a plain invocation falls through to boot. Bloomery does not
    // depend on the `aether-chassis` aggregate, so it hands its own crate's
    // `build.rs` provenance to the shared prelude directly, and it has no
    // `--print-config` flag (no chassis config file), so that mode is `false`.
    if run_chassis_prelude::<BloomeryChassis>(
        PreludeFlags { describe: cli.describe, print_config: false },
        &BloomeryChassis::build_provenance(),
    )?
    .is_handled()
    {
        return Ok(());
    }
    if cli.doctor {
        let report = KitReport::inspect();
        io::stdout().write_all(report.render_doctor().as_bytes())?;
        if !report.is_ready() {
            process::exit(1);
        }
        return Ok(());
    }
    if cli.check_store {
        let env = BloomeryEnv::resolve(&cli)?;
        let check = check_store(&env.store.path)?;
        io::stdout().write_all(check.render().as_bytes())?;
        if !check.is_clean() {
            process::exit(1);
        }
        return Ok(());
    }
    let env = BloomeryEnv::resolve(&cli)?;
    // `build` installs the substrate tracing subscriber, so this logs.
    let chassis = BloomeryChassis::build(env)?;
    tracing::info!("aether-chassis-bloomery: bloomery chassis initialised (profile={})", BloomeryChassis::PROFILE);
    chassis.run()?;
    Ok(())
}
