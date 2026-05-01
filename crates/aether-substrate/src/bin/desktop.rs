//! Desktop substrate binary entry point. See
//! `aether_substrate::desktop` for the chassis impl.

use aether_substrate::desktop::{DesktopChassis, DesktopEnv};
use aether_substrate_core::Chassis;

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
