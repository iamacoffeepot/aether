//! Hub chassis binary entry point. The hub chassis lives in
//! `aether-hub`; this binary just reads env and runs.

// CLI diagnostic before tracing subscriber is installed (issue 891).
#![allow(clippy::print_stderr)]

use aether_substrate_bundle::hub::{Chassis, HubChassis, HubEnv};

fn main() -> anyhow::Result<()> {
    let chassis = HubChassis::build(HubEnv::from_env())?;
    eprintln!(
        "aether-substrate-bundle: hub chassis initialised (profile={})",
        HubChassis::PROFILE,
    );
    chassis.run()?;
    Ok(())
}
