//! `muse bootstrap`: build an environment from two imported images and move
//! its platform's head to it.
//!
//! The same six steps as the `aether-bloomery-bootstrap` component, sent from
//! outside an engine the hub did not spawn: import the base, import the
//! toolchain, stage the merge input, call `environment.merge` in the bundle
//! bound under `workspace-programs`, read the merged environment, and move the
//! head `(aether.workspace.environment, <platform>)` to it unless it already
//! names it. Each failure names its step.

use aether_bloomery_kinds::{EncodedArtifact, RecordedHead};
use aether_bloomery_workspace::{Environment, ImageRef};
use aether_bloomery_workspace_programs::WORKSPACE_PROGRAMS;
use aether_bloomery_workspace_programs::environment::{EnvironmentMerge, MergeInput};
use aether_data::{Kind, Ref};
use anyhow::{Context, Result, anyhow};
use clap::Args;

use super::EngineArgs;
use super::bind_programs::{commit, named_move};
use super::call_in;
use crate::bloomery::read_value;

/// Arguments for `cargo xtask muse bootstrap`.
#[derive(Args, Debug)]
pub(super) struct BootstrapArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// The distro userland image, a digest-pinned reference.
    #[arg(long)]
    base: String,
    /// The Rust toolchain image, a digest-pinned reference.
    #[arg(long)]
    toolchain: String,
}

/// Build the environment and print `platform=<p>` and `environment=<digest>`,
/// the latter prefixed `unchanged ` when the platform's head already named it.
pub(super) fn run(args: &BootstrapArgs) -> Result<()> {
    let base = ImageRef::new(args.base.as_str()).map_err(|error| anyhow!("--base: {error}"))?;
    let toolchain = ImageRef::new(args.toolchain.as_str()).map_err(|error| anyhow!("--toolchain: {error}"))?;
    let mut engine = args.engine.connect()?;

    let base = engine.import(&base).context("importing the base image")?;
    let toolchain = engine.import(&toolchain).context("importing the toolchain image")?;

    let input = EncodedArtifact::new(&MergeInput { base, toolchain }).context("encoding the merge input")?;
    let input_ref = Ref::from_digest(input.digest());
    engine.stage_artifacts(vec![input]).context("staging the merge input")?;

    let (seq, result) =
        call_in::<EnvironmentMerge>(&mut engine, WORKSPACE_PROGRAMS, input_ref).context("calling environment.merge")?;
    let environment = read_value::<Environment>(&mut engine, result).context("reading the merged environment")?;

    let head = RecordedHead::new(Environment::ID, environment.platform.as_str())
        .map_err(|error| anyhow!("the platform does not name a head: {error}"))?;
    let publish = named_move(&mut engine, head, result, seq).context("reading the environment head")?;
    let moved = publish.is_some();
    if let Some(publish) = publish {
        commit(&mut engine, publish).context("moving the environment head")?;
    }

    println!("platform={}", environment.platform.as_str());
    println!(
        "environment={}{result}",
        if moved {
            ""
        } else {
            "unchanged "
        }
    );
    Ok(())
}
