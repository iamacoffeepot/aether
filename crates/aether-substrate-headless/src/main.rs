// Headless chassis entry point.
//
// Reads chassis-relevant env vars into a `HeadlessEnv`, asks
// `HeadlessChassis` to build itself (substrate-core internals, nop
// chassis sinks, native capabilities, optional hub, std-timer
// driver), and blocks on the resulting chassis until the process
// exits. Per ADR-0071 phase 5 the timer loop body lives in
// `driver::HeadlessTimerCapability`; this binary is the env-reading
// edge plus a logging line plus the run call.

mod chassis;
mod driver;

use chassis::{HeadlessChassis, HeadlessEnv};

use aether_substrate_core::Chassis;

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
