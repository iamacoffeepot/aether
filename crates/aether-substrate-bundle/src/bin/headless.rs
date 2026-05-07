//! Headless substrate binary entry point.

use aether_substrate::Chassis;
use aether_substrate_bundle::headless::{HeadlessChassis, HeadlessEnv};

fn main() -> anyhow::Result<()> {
    let env = HeadlessEnv::from_env();
    let chassis = HeadlessChassis::build(env)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = HeadlessChassis::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}
