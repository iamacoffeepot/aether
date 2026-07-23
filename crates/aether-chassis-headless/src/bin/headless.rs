//! Headless substrate binary entry point.
//!
//! Parses argv with [`HeadlessCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution.

use aether_chassis::cli::HeadlessCli;
use aether_chassis::run_describe_prelude;
use aether_chassis_headless::{HeadlessChassis, HeadlessEnv};
use aether_substrate::Chassis;
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = HeadlessCli::parse();
    // ADR-0162 shared prelude: `--print-config` (ADR-0090 §4 dump) and
    // `--describe` (ADR-0115 manifest) print and exit before Init; a plain
    // invocation falls through to boot.
    if run_describe_prelude::<HeadlessChassis>(&cli.meta)?.is_handled() {
        return Ok(());
    }
    let env = HeadlessEnv::resolve(cli)?;
    let chassis = HeadlessChassis::build(env)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = HeadlessChassis::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}
