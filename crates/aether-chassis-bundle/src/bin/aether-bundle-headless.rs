//! Generic headless bundle binary (iamacoffeepot/aether#1529).
//!
//! A headless substrate that auto-loads the component pack embedded at
//! build time, so the binary runs hub-less — a self-contained tool /
//! server build. `cargo xtask bundle --chassis headless` builds the
//! component wasms and embeds them via the crate `build.rs` (see
//! `AETHER_BUNDLE_MANIFEST`); a plain build embeds an empty-pack
//! placeholder.

use anyhow::Context as _;

use aether_chassis::TickConfig;
use aether_chassis::autoload::expand_replicas;
use aether_chassis::boot::CommonEnv;
use aether_chassis::bundle_pack::decode_pack;
use aether_chassis_headless::{HeadlessChassis, HeadlessCli};
use aether_substrate::Chassis;

/// The component pack, embedded at build time. `build.rs` stages it
/// into `OUT_DIR/bundle_pack.bin` from `AETHER_BUNDLE_MANIFEST` (the
/// bundle flow) or an empty-pack placeholder (a normal build).
const PACK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bundle_pack.bin"));

fn main() -> anyhow::Result<()> {
    let pack = decode_pack(PACK).context("decode embedded bundle pack")?;
    // Resolve the chassis env as the headless bin does — so an injected
    // `AETHER_RPC_PORT` still wires up — then overlay the pack's chassis
    // settings and queue the embedded components.
    let mut env = CommonEnv::resolve(HeadlessCli::default())?;
    // ADR-0162: the tick cadence resolves in `Chassis::build` off the base's
    // source stack, so the pack's `tick_hz` overlays as a top-priority
    // programmatic `TickConfig` override on that stack (`build` lowers it to the
    // timer period).
    if let Some(hz) = pack.chassis.tick_hz.filter(|hz| *hz > 0) {
        env.base.sources.set_override(TickConfig { hz });
    }
    if pack.chassis.title.is_some() || pack.chassis.window_mode.is_some() {
        tracing::warn!(
            target: "aether_substrate::boot",
            "bundle pack sets title/window_mode, which the headless chassis ignores (no window)",
        );
    }
    if pack.components.is_empty() {
        tracing::warn!(
            target: "aether_substrate::boot",
            "empty bundle pack — booting componentless (build through `cargo xtask bundle`)",
        );
    }
    let mut autoload = Vec::with_capacity(pack.components.len());
    for packed in pack.components {
        autoload.extend(expand_replicas(packed).context("expand bundle pack replicas")?);
    }
    env.autoload = autoload;
    let chassis = HeadlessChassis::build(env)?;
    chassis.run()?;
    Ok(())
}
