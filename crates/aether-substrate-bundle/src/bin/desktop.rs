//! Desktop substrate binary entry point. See
//! `aether_substrate_bundle::desktop` for the chassis impl.

use aether_substrate::Chassis;
use aether_substrate_bundle::desktop::{DesktopChassis, DesktopEnv};

fn main() -> anyhow::Result<()> {
    let env = DesktopEnv::from_env()?;
    let chassis = DesktopChassis::build(env)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = DesktopChassis::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}
