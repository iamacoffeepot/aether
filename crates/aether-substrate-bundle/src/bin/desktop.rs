//! Desktop substrate binary entry point. See
//! `aether_substrate_bundle::desktop` for the chassis impl.
//!
//! Parses argv with [`DesktopCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution.

// `--config` prints the discovery dump to stdout before boot
// (ADR-0090 §4 / e2).
#![allow(clippy::print_stdout)]

use aether_substrate::Chassis;
use aether_substrate_bundle::chassis_config_dump;
use aether_substrate_bundle::cli::DesktopCli;
use aether_substrate_bundle::desktop::{DesktopChassis, DesktopEnv};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = DesktopCli::parse();
    if cli.config {
        print!("{}", chassis_config_dump());
        return Ok(());
    }
    let env = DesktopEnv::from_env_with_argv(cli)?;
    let chassis = DesktopChassis::build(env)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = DesktopChassis::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}
