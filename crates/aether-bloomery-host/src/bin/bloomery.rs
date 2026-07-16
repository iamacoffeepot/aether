//! The `bloomery` binary: boot [`BloomeryChassis`] and run until a shutdown
//! signal. Every knob resolves from `AETHER_*` env (the engines cap injects
//! `AETHER_RPC_PORT`; `AETHER_STORE_PATH` points at the durable database file).

use aether_bloomery_host::bloomery::{BloomeryChassis, BloomeryEnv, Chassis};

fn main() -> anyhow::Result<()> {
    let env = BloomeryEnv::from_env()?;
    // `build` installs the substrate tracing subscriber, so this logs.
    let chassis = BloomeryChassis::build(env)?;
    tracing::info!("aether-bloomery-host: bloomery chassis initialised (profile={})", BloomeryChassis::PROFILE);
    chassis.run()?;
    Ok(())
}
