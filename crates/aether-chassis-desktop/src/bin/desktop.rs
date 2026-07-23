//! Desktop substrate binary entry point. See
//! `aether_chassis_desktop` for the chassis impl.
//!
//! Parses argv with [`DesktopCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution.

use aether_chassis::run_describe_prelude;
use aether_chassis_desktop::{DesktopChassis, DesktopCli, DesktopEnv};
use aether_substrate::Chassis;
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = DesktopCli::parse();
    // ADR-0162 shared prelude: `--print-config` (ADR-0090 §4 dump) and
    // `--describe` (ADR-0115 manifest) print and exit before Init; a plain
    // invocation falls through to boot (no winit event loop opened until then).
    if run_describe_prelude::<DesktopChassis>(&cli.meta)?.is_handled() {
        return Ok(());
    }
    let env = DesktopEnv::resolve(cli)?;
    let chassis = DesktopChassis::build(env)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = DesktopChassis::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}
