//! `muse bind-programs`: bind the `workspace-programs` bundle under its head.
//!
//! The bundle holds the environment merge, the cargo vendor run, and the
//! proofs. It has no reactors, so unlike `bind` this moves one head and leaves
//! the reactor set alone. A rebind of the same bundle publishes nothing, so it
//! appends no head move.

use std::fs;
use std::path::PathBuf;
use std::slice;

use aether_bloomery_kinds::{EncodedArtifact, Publish, PublishResult, RecordedHead, RecordedHeadMove};
use aether_bloomery_workspace_programs::WORKSPACE_PROGRAMS;
use aether_data::Digest;
use anyhow::{Context, Result, bail};
use clap::Args;

use super::EngineArgs;
use crate::bloomery::{Engine, Reads, latest_moves, publish_at_fence};

/// Arguments for `cargo xtask muse bind-programs`.
#[derive(Args, Debug)]
pub(super) struct BindProgramsArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// The built workspace-programs bundle: the `aether_bloomery_workspace_programs` wasm.
    #[arg(long)]
    bundle: PathBuf,
}

/// Bind the bundle and print `bound bundle=<digest>`, or `unchanged
/// bundle=<digest>` when the head already names it.
pub(super) fn run(args: &BindProgramsArgs) -> Result<()> {
    let wasm = fs::read(&args.bundle).with_context(|| format!("reading the bundle {}", args.bundle.display()))?;
    let mut engine = args.engine.connect()?;

    let bundle = EncodedArtifact::opaque_bytes(&wasm);
    let digest = bundle.digest();
    let fence = engine.read_head()?;
    let Some(publish) = programs_publish(&mut engine, digest, fence)? else {
        println!("unchanged bundle={digest}");
        return Ok(());
    };

    engine.stage_artifacts(vec![bundle])?;
    commit(&mut engine, publish).context("moving the workspace-programs head")?;
    println!("bound bundle={digest}");
    Ok(())
}

/// The publish that moves the `workspace-programs` head to `digest` at
/// `fence`, or `None` when the head already names it.
pub(super) fn programs_publish(reads: &mut impl Reads, digest: Digest, fence: u64) -> Result<Option<Publish>> {
    named_move(reads, RecordedHead::from(&WORKSPACE_PROGRAMS), digest, fence)
}

/// The publish that moves `head` to `to` at `fence`, or `None` when the
/// journal's latest move of `head` already names `to`.
pub(super) fn named_move(
    reads: &mut impl Reads,
    head: RecordedHead,
    to: Digest,
    fence: u64,
) -> Result<Option<Publish>> {
    let bound = latest_moves(reads, slice::from_ref(&head))?.into_iter().next().flatten();
    let moves = vec![RecordedHeadMove::new(head, to)];
    Ok((bound != Some(to)).then(|| Publish::new(Vec::new(), moves, fence)))
}

/// Publish `publish`, resending it at the journal's head on a stale fence, and
/// fail unless the journal commits it.
pub(super) fn commit(engine: &mut Engine, publish: Publish) -> Result<()> {
    match publish_at_fence(engine, publish)? {
        PublishResult::Committed { .. } => Ok(()),
        PublishResult::Conflict { actual } => {
            bail!("the journal reported a conflict at the fence {actual} it was sent")
        }
        PublishResult::Err { message } => bail!("the journal refused the head move: {message}"),
    }
}
