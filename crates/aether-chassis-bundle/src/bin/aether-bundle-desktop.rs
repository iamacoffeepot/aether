//! Generic desktop bundle binary (iamacoffeepot/aether#1529).
//!
//! A desktop substrate that auto-loads the component pack embedded at
//! build time, so the binary runs hub-less and double-click-to-play.
//! No MCP, no hub, no netcode. `cargo xtask bundle --chassis desktop`
//! builds the component wasms and embeds them via the crate `build.rs`
//! (see `AETHER_BUNDLE_MANIFEST`); a plain build embeds an empty-pack
//! placeholder.

use anyhow::Context as _;

use aether_chassis::WindowConfig;
use aether_chassis::autoload::expand_replicas;
use aether_chassis::boot::CommonEnv;
use aether_chassis::bundle_pack::decode_pack;
use aether_chassis_desktop::{DesktopChassis, DesktopCli};
use aether_substrate::Chassis;

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
    let mut env = CommonEnv::resolve(DesktopCli::default())?;
    // ADR-0162: the window boot knobs resolve in `Chassis::build` off the base's
    // source stack, so the pack's window settings overlay as a top-priority
    // programmatic override on that stack (beating env, matching the pre-refactor
    // post-resolution overwrite). `build`'s `WindowConfig::lower` parses the
    // `window_mode` spec, so the bundle no longer parses it itself.
    if pack.chassis.title.is_some() || pack.chassis.window_mode.is_some() {
        let mut window = env.base.sources.resolve::<WindowConfig>()?;
        if let Some(title) = pack.chassis.title {
            window.title = Some(title);
        }
        if let Some(spec) = pack.chassis.window_mode {
            window.mode = Some(spec);
        }
        env.base.sources.set_override(window);
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
    let mut autoload = Vec::with_capacity(pack.components.len());
    for packed in pack.components {
        autoload.extend(expand_replicas(packed).context("expand bundle pack replicas")?);
    }
    env.autoload = autoload;
    let chassis = DesktopChassis::build(env)?;
    chassis.run()?;
    Ok(())
}
