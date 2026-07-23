//! Generic desktop bundle binary (iamacoffeepot/aether#1529).
//!
//! A desktop substrate that auto-loads the component package embedded at build
//! time, so the binary runs hub-less and double-click-to-play. No MCP, no hub,
//! no netcode. `cargo xtask bundle --chassis desktop` builds the component
//! wasms and embeds the depot-shaped package artifact via the crate `build.rs`
//! (see `AETHER_BUNDLE_PACK`); a plain build embeds an empty manifest and boots
//! componentless. Boot decodes the embedded `pack/manifest` and resolves each
//! entry's wasm + config against the embedded object table through the same
//! `aether_chassis::package` path the depot boot uses (ADR-0163 §1).

use aether_chassis::WindowConfig;
use aether_chassis::autoload::AutoloadComponent;
use aether_chassis::boot::CommonEnv;
use aether_chassis::bundle_pack::ChassisSettings;
use aether_chassis::package::{EmbeddedObjectStore, embedded_autoload};
use aether_chassis_desktop::{DesktopChassis, DesktopCli};
use aether_substrate::Chassis;

/// The embedded package artifact: the `pack/manifest` bytes plus the object
/// table (`(hex, bytes)` per `pack/objects/<sha256>`). `build.rs` generates
/// this from `AETHER_BUNDLE_PACK` (the bundle flow) or as an empty placeholder
/// (a plain build).
mod embedded_pack {
    include!(concat!(env!("OUT_DIR"), "/embedded_pack.rs"));
}

fn main() -> anyhow::Result<()> {
    let (settings, autoload) = load_embedded_package()?;

    // Resolve the chassis env as the desktop bin does — so an injected
    // `AETHER_RPC_PORT` (e.g. when the hub spawns this for a capture) still
    // wires up — then overlay the package's chassis settings and queue the
    // embedded components.
    let mut env = CommonEnv::resolve(DesktopCli::default())?;
    // ADR-0162: the window boot knobs resolve in `Chassis::build` off the base's
    // source stack, so the package's window settings overlay as a top-priority
    // programmatic override on that stack (beating env). `build`'s
    // `WindowConfig::lower` parses the `window_mode` spec, so the bundle no
    // longer parses it itself.
    if settings.title.is_some() || settings.window_mode.is_some() {
        let mut window = env.base.sources.resolve::<WindowConfig>()?;
        if let Some(title) = settings.title {
            window.title = Some(title);
        }
        if let Some(spec) = settings.window_mode {
            window.mode = Some(spec);
        }
        env.base.sources.set_override(window);
    }
    if settings.tick_hz.is_some() {
        tracing::warn!(
            target: "aether_substrate::boot",
            "embedded package sets tick_hz, which the desktop chassis ignores (frame-driven ticks)",
        );
    }
    env.autoload = autoload;
    let chassis = DesktopChassis::build(env)?;
    chassis.run()?;
    Ok(())
}

/// Decode the embedded package into chassis settings + the autoload list. An
/// empty embedded manifest (a plain build) is the componentless placeholder.
fn load_embedded_package() -> anyhow::Result<(ChassisSettings, Vec<AutoloadComponent>)> {
    if embedded_pack::MANIFEST.is_empty() {
        tracing::warn!(
            target: "aether_substrate::boot",
            "empty embedded package — booting componentless (build through `cargo xtask bundle`)",
        );
        return Ok((ChassisSettings::default(), Vec::new()));
    }
    let objects = EmbeddedObjectStore::new(embedded_pack::OBJECTS);
    Ok(embedded_autoload(embedded_pack::MANIFEST, &objects)?)
}
