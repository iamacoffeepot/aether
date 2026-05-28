//! Hub chassis binary entry point. The hub chassis lives in
//! `aether-hub`; this binary just reads argv-then-env and runs.
//!
//! Parses argv with [`HubCli`] (ADR-0090 unit d, issue 1258);
//! `--rpc-port` shadows `AETHER_RPC_PORT`.

// CLI diagnostic before tracing subscriber is installed (issue 891).
#![allow(clippy::print_stderr)]

use aether_substrate_bundle::cli::HubCli;
use aether_substrate_bundle::hub::{Chassis, HubChassis, HubEnv};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = HubCli::parse();
    let chassis = HubChassis::build(HubEnv::from_env_with_argv(&cli))?;
    eprintln!(
        "aether-substrate-bundle: hub chassis initialised (profile={})",
        HubChassis::PROFILE,
    );
    chassis.run()?;
    Ok(())
}
