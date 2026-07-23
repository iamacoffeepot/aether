//! Generic headless bundle binary (iamacoffeepot/aether#1529).
//!
//! A headless substrate that auto-loads the component package embedded at
//! build time, so the binary runs hub-less — a self-contained tool / server
//! build. `cargo xtask bundle --chassis headless` builds the component wasms
//! and embeds the depot-shaped package artifact via the crate `build.rs` (see
//! `AETHER_BUNDLE_PACK`); a plain build embeds an empty manifest and boots
//! componentless. Boot decodes the embedded `pack/manifest` and resolves each
//! entry's wasm + config against the embedded object table through the same
//! `aether_chassis::package` path the depot boot uses (ADR-0163 §1).

use aether_chassis::TickConfig;
use aether_chassis::autoload::AutoloadComponent;
use aether_chassis::boot::CommonEnv;
use aether_chassis::bundle_pack::ChassisSettings;
use aether_chassis::package::{EmbeddedObjectStore, embedded_autoload};
use aether_chassis_headless::{HeadlessChassis, HeadlessCli};
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

    // Resolve the chassis env as the headless bin does — so an injected
    // `AETHER_RPC_PORT` still wires up — then overlay the package's chassis
    // settings and queue the embedded components.
    let mut env = CommonEnv::resolve(HeadlessCli::default())?;
    // ADR-0162: the tick cadence resolves in `Chassis::build` off the base's
    // source stack, so the package's `tick_hz` overlays as a top-priority
    // programmatic `TickConfig` override on that stack (`build` lowers it to the
    // timer period).
    if let Some(hz) = settings.tick_hz.filter(|hz| *hz > 0) {
        env.base.sources.set_override(TickConfig { hz });
    }
    if settings.title.is_some() || settings.window_mode.is_some() {
        tracing::warn!(
            target: "aether_substrate::boot",
            "embedded package sets title/window_mode, which the headless chassis ignores (no window)",
        );
    }
    env.autoload = autoload;
    let chassis = HeadlessChassis::build(env)?;
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
