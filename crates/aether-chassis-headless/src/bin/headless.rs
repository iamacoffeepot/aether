//! Headless substrate binary entry point.
//!
//! Parses argv with [`HeadlessCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution.

// `--print-config` prints the discovery dump to stdout before tracing is up
// (ADR-0090 §4 / e2).
#![allow(clippy::print_stdout)]

use aether_chassis::cli::HeadlessCli;
use aether_chassis_headless::{HeadlessChassis, HeadlessEnv};
use aether_substrate::Chassis;
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = HeadlessCli::parse();
    if cli.print_config {
        // ADR-0156 §4: the dump is the composition-derived aggregate plus the
        // residual hand records, so it resolves the chassis config the same
        // way a boot does (a garbage known value surfaces as a `ConfigError`).
        print!("{}", aether_chassis::config_dump::<HeadlessChassis>()?);
        return Ok(());
    }
    // `--describe` (ADR-0115, issue 1953): print this binary's manifest —
    // chassis kind, linked caps, build provenance — as JSON, then exit
    // before boot. The hub's binary store forks `<binary> --describe`
    // once at upload time to capture exactly this.
    if cli.describe {
        println!("{}", serde_json::to_string(&aether_chassis::describe_manifest::<HeadlessChassis>()?)?);
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
