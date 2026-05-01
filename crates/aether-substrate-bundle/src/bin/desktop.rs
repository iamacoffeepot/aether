//! Desktop substrate binary entry point. See
//! `aether_substrate_bundle::desktop` for the chassis impl.

use aether_substrate::Chassis;
use aether_substrate_bundle::desktop::{DesktopChassis, DesktopEnv};

fn main() -> wasmtime::Result<()> {
    let env = DesktopEnv::from_env()?;
    let chassis = DesktopChassis::build(env)
        .map_err(|e| wasmtime::Error::msg(format!("chassis build: {e}")))?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = DesktopChassis::PROFILE,
        "chassis initialised",
    );
    chassis
        .run()
        .map_err(|e| wasmtime::Error::msg(format!("chassis run: {e}")))
}
