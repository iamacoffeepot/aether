//! Headless substrate binary entry point.
//!
//! Parses argv with [`HeadlessCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution.

// `--config` prints the discovery dump to stdout before tracing is up
// (ADR-0090 §4 / e2).
#![allow(clippy::print_stdout)]

use aether_substrate::Chassis;
use aether_substrate_bundle::chassis_config_dump;
use aether_substrate_bundle::cli::HeadlessCli;
use aether_substrate_bundle::headless::{HeadlessChassis, HeadlessEnv};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = HeadlessCli::parse();
    if cli.config {
        print!("{}", chassis_config_dump());
        return Ok(());
    }
    let env = HeadlessEnv::from_env_with_argv(cli)?;
    let chassis = HeadlessChassis::build(env)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = HeadlessChassis::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}
