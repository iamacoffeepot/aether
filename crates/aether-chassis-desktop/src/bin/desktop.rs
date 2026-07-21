//! Desktop substrate binary entry point. See
//! `aether_chassis_desktop` for the chassis impl.
//!
//! Parses argv with [`DesktopCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution.

// `--print-config` prints the discovery dump to stdout before boot
// (ADR-0090 §4 / e2).
#![allow(clippy::print_stdout)]

use aether_chassis::cli::DesktopCli;
use aether_chassis_desktop::{DesktopChassis, DesktopEnv};
use aether_substrate::Chassis;
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = DesktopCli::parse();
    if cli.print_config {
        // ADR-0156 §4: the dump is the composition-derived aggregate plus the
        // residual hand records, so it resolves the chassis config the same
        // way a boot does (a garbage known value surfaces as a `ConfigError`).
        print!("{}", DesktopChassis::config_dump()?);
        return Ok(());
    }
    // `--describe` (ADR-0115, issue 1953): print this binary's manifest —
    // chassis kind, linked caps, build provenance — as JSON, then exit
    // before boot (no winit event loop opened).
    if cli.describe {
        println!("{}", serde_json::to_string(&DesktopChassis::describe_manifest()?)?);
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
