//! Hub chassis binary entry point. The hub chassis lives in
//! `aether-hub`; this binary just reads env and runs.

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
