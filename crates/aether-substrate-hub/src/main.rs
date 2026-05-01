//! Hub chassis binary entry point.
//!
//! Reads chassis-relevant env vars into a [`HubEnv`], asks the
//! [`HubChassis`] to build itself (engine + session + spawn + log
//! stores, in-process loopback substrate, [`HubServerDriverCapability`]
//! driver), and blocks on the resulting chassis until either listener
//! exits or a SIGINT/SIGTERM arrives. ADR-0071 phase 7d collapsed
//! every step the prior `main()` body did inline (and the further
//! steps in `HubChassis::run_async`) into the chassis build path;
//! this binary is now the env-reading edge plus a logging line plus
//! the run call.

use aether_hub::{Chassis, HubChassis, HubEnv};

fn main() -> wasmtime::Result<()> {
    let chassis = HubChassis::build(HubEnv::from_env())
        .map_err(|e| wasmtime::Error::msg(format!("chassis build: {e}")))?;
    eprintln!(
        "aether-substrate-hub: chassis initialised (profile={})",
        HubChassis::PROFILE,
    );
    chassis
        .run()
        .map_err(|e| wasmtime::Error::msg(format!("chassis run: {e}")))
}
