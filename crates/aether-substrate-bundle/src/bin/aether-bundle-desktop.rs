//! Generic desktop bundle binary (iamacoffeepot/aether#1529).
//!
//! A desktop substrate that auto-loads the component pack embedded at
//! build time, so the binary runs hub-less and double-click-to-play.
//! No MCP, no hub, no netcode. `cargo xtask bundle --chassis desktop`
//! builds the component wasms and embeds them via the crate `build.rs`
//! (see `AETHER_BUNDLE_MANIFEST`); a plain build embeds an empty-pack
//! placeholder.

use anyhow::Context as _;

use aether_substrate::Chassis;
use aether_substrate_bundle::bundle_pack::decode_pack;
use aether_substrate_bundle::desktop::driver::parse_window_mode_env;
use aether_substrate_bundle::desktop::{AutoloadComponent, DesktopChassis, DesktopEnv};

/// The component pack, embedded at build time. `build.rs` stages it
/// into `OUT_DIR/bundle_pack.bin` from `AETHER_BUNDLE_MANIFEST` (the
/// bundle flow) or an empty-pack placeholder (a normal build).
const PACK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bundle_pack.bin"));

fn main() -> anyhow::Result<()> {
    let pack = decode_pack(PACK).context("decode embedded bundle pack")?;
    // Resolve the chassis env as the desktop bin does — so an injected
    // `AETHER_RPC_PORT` (e.g. when the hub spawns this for a capture) still
    // wires up — then overlay the pack's chassis settings and queue the
    // embedded components.
    let mut env = DesktopEnv::from_env()?;
    if let Some(title) = pack.chassis.title {
        env.boot_title = title;
    }
    if let Some(spec) = pack.chassis.window_mode {
        let (mode, size) = parse_window_mode_env(&spec)
            .map_err(|e| anyhow::anyhow!("bundle pack window_mode {spec:?} unparseable: {e}"))?;
        env.boot_mode = mode;
        env.boot_size = size;
    }
    if pack.chassis.tick_hz.is_some() {
        tracing::warn!(
            target: "aether_substrate::boot",
            "bundle pack sets tick_hz, which the desktop chassis ignores (frame-driven ticks)",
        );
    }
    if pack.components.is_empty() {
        tracing::warn!(
            target: "aether_substrate::boot",
            "empty bundle pack — booting componentless (build through `cargo xtask bundle`)",
        );
    }
    env.autoload = pack
        .components
        .into_iter()
        .map(AutoloadComponent::from)
        .collect();
    let chassis = DesktopChassis::build(env)?;
    chassis.run()?;
    Ok(())
}
