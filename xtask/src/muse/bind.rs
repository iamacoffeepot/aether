//! `muse bind`: bind the muse bundle under its head and a reactor set that
//! runs its session loop, once per engine.
//!
//! The bundle bytes and the new set are staged first; then one fenced publish
//! moves only the heads that do not already name them. The set keeps every
//! member the bound set already holds and adds [`MUSE`], so binding never
//! drops another bundle's reactors. A rebind of the same bundle publishes
//! nothing, so it appends no head move.

use std::fs;
use std::path::PathBuf;

use aether_bloomery_kinds::{EncodedArtifact, Publish, PublishResult, ReactorSet, RecordedHead, RecordedHeadMove};
use aether_bloomery_muse::MUSE;
use anyhow::{Context, Result, anyhow, bail};
use clap::Args;

use super::EngineArgs;
use crate::bloomery::{latest_moves, publish_at_fence, read_value};

/// Arguments for `cargo xtask muse bind`.
#[derive(Args, Debug)]
pub(super) struct BindArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// The built muse bundle: the `aether_bloomery_muse` wasm.
    #[arg(long)]
    bundle: PathBuf,
}

/// Bind the bundle and print `bound` or `unchanged` with the bundle and set
/// digests.
pub(super) fn run(args: &BindArgs) -> Result<()> {
    let wasm = fs::read(&args.bundle).with_context(|| format!("reading the bundle {}", args.bundle.display()))?;
    let mut engine = args.engine.connect()?;

    let (muse, root) = (RecordedHead::from(&MUSE), RecordedHead::from(&ReactorSet::ROOT));
    let [bound_bundle, bound_set] = <[_; 2]>::try_from(latest_moves(&mut engine, &[muse.clone(), root.clone()])?)
        .map_err(|_| anyhow!("the head read answered other than two heads"))?;

    let mut members = match bound_set {
        Some(set) => read_value::<ReactorSet>(&mut engine, set)?.clusters().to_vec(),
        None => Vec::new(),
    };
    if !members.contains(&MUSE) {
        members.push(MUSE);
        members.sort();
    }
    let bundle = EncodedArtifact::opaque_bytes(&wasm);
    let set = EncodedArtifact::new(&ReactorSet::new(members).map_err(|error| anyhow!("the reactor set: {error}"))?)?;
    let (bundle_digest, set_digest) = (bundle.digest(), set.digest());

    let moves: Vec<_> = [(muse, bound_bundle, bundle_digest), (root, bound_set, set_digest)]
        .into_iter()
        .filter(|(_, bound, to)| *bound != Some(*to))
        .map(|(head, _, to)| RecordedHeadMove::new(head, to))
        .collect();
    if moves.is_empty() {
        println!("unchanged bundle={bundle_digest} set={set_digest}");
        return Ok(());
    }

    engine.stage_artifacts(vec![bundle, set])?;
    let fence = engine.read_head()?;
    match publish_at_fence(&mut engine, Publish::new(Vec::new(), moves, fence))? {
        PublishResult::Committed { .. } => println!("bound bundle={bundle_digest} set={set_digest}"),
        PublishResult::Conflict { actual } => {
            bail!("the journal reported a conflict at the fence {actual} it was sent")
        }
        PublishResult::Err { message } => bail!("the journal refused the head moves: {message}"),
    }
    Ok(())
}
