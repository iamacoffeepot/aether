//! Headless substrate binary entry point.

use aether_substrate::Chassis;
use aether_substrate_bundle::headless::{HeadlessChassis, HeadlessEnv};

fn main() -> wasmtime::Result<()> {
    let env = HeadlessEnv::from_env();
    let chassis = HeadlessChassis::build(env)
        .map_err(|e| wasmtime::Error::msg(format!("chassis build: {e}")))?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = HeadlessChassis::PROFILE,
        "chassis initialised",
    );
    chassis
        .run()
        .map_err(|e| wasmtime::Error::msg(format!("chassis run: {e}")))
}
