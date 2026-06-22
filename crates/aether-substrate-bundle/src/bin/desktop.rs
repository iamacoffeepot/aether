//! Desktop substrate binary entry point. See
//! `aether_substrate_bundle::desktop` for the chassis impl.
//!
//! Parses argv with [`DesktopCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution.

// `--config` prints the discovery dump to stdout before boot
// (ADR-0090 §4 / e2).
#![allow(clippy::print_stdout)]

// Force-link `aether-labyrinth` into this engine binary so its certifier
// `#[transform]`s register in the link-time `aether_data::transforms()`
// inventory the chassis's `TransformRegistry` is built from (issue 1908) —
// the registry the `aether.fs` `fetch` verb resolves a caller's transform
// chain against. The bundle lib references only the recorder cap (a
// different codegen member), so without this the linker drops the
// transforms member and those transforms silently vanish from the registry
// with no compile error. `as _` is the side-effect-linkage form, and the
// directive must live in this final-artifact root — it does not propagate
// in from the bundle lib's own `extern crate`.
extern crate aether_labyrinth as _;

use aether_substrate::Chassis;
use aether_substrate_bundle::chassis_config_dump;
use aether_substrate_bundle::cli::DesktopCli;
use aether_substrate_bundle::desktop::{DesktopChassis, DesktopEnv};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = DesktopCli::parse();
    if cli.config {
        print!("{}", chassis_config_dump());
        return Ok(());
    }
    // `--describe` (ADR-0115, issue 1953): print this binary's manifest —
    // chassis kind, linked caps, build provenance — as JSON, then exit
    // before boot (no winit event loop opened).
    if cli.describe {
        println!(
            "{}",
            serde_json::to_string(&DesktopChassis::describe_manifest())?
        );
        return Ok(());
    }
    let env = DesktopEnv::from_env_with_argv(cli)?;
    let chassis = DesktopChassis::build(env)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = DesktopChassis::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}
