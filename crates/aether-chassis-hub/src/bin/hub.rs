//! Hub chassis binary entry point. The hub chassis lives in
//! `aether-chassis-hub`; this binary just reads argv-then-env and runs.
//!
//! Parses argv with [`HubCli`] (ADR-0090 unit d, issue 1258);
//! `--rpc-port` shadows `AETHER_RPC_PORT`.

// CLI diagnostic before tracing subscriber is installed (issue 891).
#![allow(clippy::print_stderr)]

use aether_chassis::cli::HubCli;
use aether_chassis::{PreludeFlags, run_describe_prelude};
use aether_chassis_hub::{Chassis, HubChassis, HubEnv};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = HubCli::parse();
    // ADR-0162 shared prelude: `--print-config` (ADR-0090 §4 dump) and
    // `--describe` (ADR-0115 manifest) print and exit before Init; a plain
    // invocation falls through to boot.
    if run_describe_prelude::<HubChassis>(PreludeFlags { describe: cli.describe, print_config: cli.print_config })?
        .is_handled()
    {
        return Ok(());
    }
    let chassis = HubChassis::build(HubEnv::from_env_with_argv(&cli)?)?;
    eprintln!("aether-chassis-hub: hub chassis initialised (profile={})", HubChassis::PROFILE);
    chassis.run()?;
    Ok(())
}
