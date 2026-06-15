//! Headless substrate binary entry point.
//!
//! Parses argv with [`HeadlessCli`] (ADR-0090 unit d, issue 1258);
//! each per-cap overlay shadows its `AETHER_*` env var, unset flags
//! fall through to env-only resolution.

// `--config` prints the discovery dump to stdout before tracing is up
// (ADR-0090 §4 / e2).
#![allow(clippy::print_stdout)]

// Force-link `aether-labyrinth` into this engine binary so its certifier
// `#[transform]`s register in the link-time `aether_data::transforms()`
// inventory the headless chassis's `DagCapability` builds its
// `TransformRegistry` from (issue 1908). The bundle lib references only the
// recorder cap (a different codegen member), so without this the linker
// drops the transforms member and a reachability DAG fails validation with
// no compile error. `as _` is the side-effect-linkage form, and the
// directive must live in this final-artifact root — it does not propagate
// in from the bundle lib's own `extern crate`.
extern crate aether_labyrinth as _;

use aether_substrate::Chassis;
use aether_substrate_bundle::chassis_config_dump;
use aether_substrate_bundle::cli::HeadlessCli;
use aether_substrate_bundle::headless::{HeadlessChassis, HeadlessEnv};
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = HeadlessCli::parse();
    if cli.config {
        print!("{}", chassis_config_dump());
        return Ok(());
    }
    let env = HeadlessEnv::from_env_with_argv(cli)?;
    let chassis = HeadlessChassis::build(env)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = HeadlessChassis::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}
