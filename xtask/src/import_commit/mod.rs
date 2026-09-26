//! `cargo xtask import-commit <commit> --rpc-port <port>`: stage the files one
//! commit tracks as a journal tree (ADR-0237 decision 3).
//!
//! The lane runs outside every engine. It reads exactly what the commit
//! tracks, through `git ls-tree` and one `git cat-file --batch`, so no
//! allowlist decides what a content-addressed journal keeps. It builds the
//! [`aether_bloomery_kinds::Tree`] artifacts bottom-up, splits them into
//! batches under the frame cap, and stages each batch through the journal's
//! fenced `aether.bloomery.journal.publish` with no head move, so the journal
//! appends no event. stdout carries exactly two lines, `commit=<sha>` and
//! `tree=<digest>`; a proof cites the tree digest, and the journal records
//! nothing about the commit.
//!
//! - [`read`] resolves the commit and reads its listing and blob bytes.
//! - [`tree`] maps Git modes onto tree entries and seals every directory.
//! - [`batch`] splits the artifacts into ordered batches under a byte budget.
//! - [`publish`] dials the engine and stages each batch at the journal fence.

mod batch;
mod publish;
mod read;
mod tree;

#[cfg(test)]
mod tests;

use std::path::Path;

use aether_codec::frame::{install_max_frame_size, max_frame_size};
use aether_rpc::FrameSizeConfig;
use anyhow::Result;
use clap::Args;

/// Arguments for `cargo xtask import-commit`.
#[derive(Args, Debug)]
pub struct ImportCommitArgs {
    /// The commit to import: any revision `git rev-parse` resolves to a commit.
    commit: String,
    /// The Bloomery engine's RPC port on 127.0.0.1.
    #[arg(long)]
    rpc_port: u16,
}

/// Import the commit's tracked files into the engine's journal and print the
/// resolved commit and the root tree digest.
///
/// The commit resolves in the repository around the working directory. A
/// batch may use half the frame cap, leaving the other half for the envelope
/// and each artifact's citations.
pub fn run(args: &ImportCommitArgs) -> Result<()> {
    install_max_frame_size(FrameSizeConfig::try_from_env()?.to_max_frame_size());

    let listing = read::read_commit(Path::new("."), &args.commit)?;
    let built = tree::build(&listing)?;
    let batches = batch::split(built.artifacts, max_frame_size() / 2)?;

    let mut journal = publish::EngineJournal::connect(args.rpc_port)?;
    let fence = journal.read_head()?;
    publish::publish(&mut journal, fence, batches)?;

    println!("commit={}", listing.commit);
    println!("tree={}", built.root);
    Ok(())
}
