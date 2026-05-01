//! Desktop substrate binary entry point.
//!
//! Reads chassis-relevant env vars into a [`DesktopEnv`], asks the
//! [`DesktopChassis`] to build itself (substrate-core internals,
//! native capabilities, render + camera sinks, optional hub
//! connection, winit driver), and blocks on the resulting chassis
//! until the event loop exits. ADR-0071 phase 3 collapsed every
//! step the prior `main()` body did inline into the chassis build
//! path; this binary is now the env-reading edge plus a logging
//! line plus the run call.
//!
//! See `aether-substrate-desktop/src/chassis.rs` for the chassis
//! build body, and `aether-substrate-desktop/src/driver.rs` for the
//! winit driver capability.
use aether_substrate_desktop::{Chassis, DesktopChassis, DesktopEnv};

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
