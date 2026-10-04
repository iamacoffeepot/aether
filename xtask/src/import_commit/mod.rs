//! `cargo xtask import-commit <commit> --rpc-port <port> --unit <key>`: stage
//! the files one commit tracks as a journal tree (ADR-0237 decision 3) in the
//! journal of the unit `<key>` names (ADR-0240 D8).
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
//! - [`publish`] stages each batch at the journal fence over the shared
//!   [`crate::bloomery`] client.

mod batch;
mod publish;
mod read;
mod tree;

#[cfg(test)]
mod tests;

use std::path::Path;

use aether_bloomery_kinds::{EncodedArtifact, UnitKey};
use aether_codec::frame::max_frame_size;
use aether_data::Digest;
use anyhow::{Context, Result};
use clap::Args;

use crate::bloomery::{self, Engine};

/// Arguments for `cargo xtask import-commit`.
#[derive(Args, Debug)]
pub struct ImportCommitArgs {
    /// The commit to import: any revision `git rev-parse` resolves to a commit.
    commit: String,
    /// The Bloomery engine's RPC port on 127.0.0.1.
    #[arg(long)]
    rpc_port: u16,
    /// The unit key of the journal to stage into: the engine's journal owner
    /// for it answers at `aether.bloomery.journal:<key>`.
    #[arg(long)]
    unit: String,
}

/// Import the commit's tracked files into the engine's journal and print the
/// resolved commit and the root tree digest.
///
/// The commit resolves in the repository around the working directory.
pub fn run(args: &ImportCommitArgs) -> Result<()> {
    let unit = UnitKey::new(&args.unit).with_context(|| format!("--unit {:?} is not a unit key", args.unit))?;
    bloomery::install_frame_cap()?;

    let imported = Imported::read(Path::new("."), &args.commit)?;
    let mut engine = Engine::connect(args.rpc_port, &unit, "xtask import-commit")?;
    let tree = imported.stage(&mut engine)?;

    println!("commit={}", tree.commit);
    println!("tree={}", tree.root);
    Ok(())
}

/// One commit's tree, built and split into publish batches, not yet staged.
pub struct Imported {
    commit: String,
    root: Digest,
    batches: Vec<Vec<EncodedArtifact>>,
}

/// A staged commit: the resolved sha and its root tree's digest.
pub struct ImportedTree {
    pub commit: String,
    pub root: Digest,
}

impl Imported {
    /// Read the files `commit` tracks in the repository at `repo` and build
    /// their tree. A batch may use half the frame cap, leaving the other half
    /// for the envelope and each artifact's citations, so the cap must be
    /// installed first.
    ///
    /// # Errors
    /// The commit does not resolve, Git failed, an entry is refused, or an
    /// artifact is over the batch budget.
    pub fn read(repo: &Path, commit: &str) -> Result<Self> {
        let listing = read::read_commit(repo, commit)?;
        let built = tree::build(&listing)?;
        let batches = batch::split(built.artifacts, max_frame_size() / 2)?;
        Ok(Self { commit: listing.commit, root: built.root, batches })
    }

    /// Stage every batch into `engine`'s journal at its current fence.
    ///
    /// # Errors
    /// The journal refused a batch or the transport failed.
    pub fn stage(self, engine: &mut Engine) -> Result<ImportedTree> {
        let fence = engine.read_head()?;
        publish::publish(engine, fence, self.batches)?;
        Ok(ImportedTree { commit: self.commit, root: self.root })
    }
}
