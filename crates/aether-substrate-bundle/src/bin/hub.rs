//! Hub chassis binary entry point. The hub chassis lives in
//! `aether-hub`; this binary just reads argv-then-env and runs.
//!
//! Parses argv with [`HubCli`] (ADR-0090 unit d, issue 1258);
//! `--rpc-port` shadows `AETHER_RPC_PORT`.

// CLI diagnostic before tracing subscriber is installed (issue 891).
// `--print-config` prints the discovery dump to stdout before boot
// (ADR-0090 §4 / e2).
#![allow(clippy::print_stderr)]
#![allow(clippy::print_stdout)]

use aether_chassis::cli::HubCli;
use aether_chassis::hub_config_dump;
use aether_substrate_bundle::hub::{Chassis, HubChassis, HubEnv};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = HubCli::parse();
    if cli.print_config {
        print!("{}", hub_config_dump());
        return Ok(());
    }
    // `--describe` (ADR-0115, issue 1953): print this binary's manifest —
    // chassis kind, linked caps, build provenance — as JSON, then exit
    // before boot.
    if cli.describe {
        println!("{}", serde_json::to_string(&HubChassis::describe_manifest())?);
        return Ok(());
    }
    let chassis = HubChassis::build(HubEnv::from_env_with_argv(&cli)?)?;
    eprintln!("aether-substrate-bundle: hub chassis initialised (profile={})", HubChassis::PROFILE);
    chassis.run()?;
    Ok(())
}
