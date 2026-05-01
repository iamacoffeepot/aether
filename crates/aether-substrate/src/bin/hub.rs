//! Hub chassis binary entry point. The hub chassis lives in
//! `aether-hub`; this binary just reads env and runs.

use aether_substrate::hub::{Chassis, HubChassis, HubEnv};

fn main() -> wasmtime::Result<()> {
    let chassis = HubChassis::build(HubEnv::from_env())
        .map_err(|e| wasmtime::Error::msg(format!("chassis build: {e}")))?;
    eprintln!(
        "aether-substrate-bundle: hub chassis initialised (profile={})",
        HubChassis::PROFILE,
    );
    chassis
        .run()
        .map_err(|e| wasmtime::Error::msg(format!("chassis run: {e}")))
}
