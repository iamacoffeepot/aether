//! `muse vendor`: run `cargo vendor` over a source tree's `Cargo.lock` in a
//! published environment and print the vendor tree.
//!
//! The source is a commit, imported as `muse open --commit` imports it, or a
//! tree already in the journal. The result is the tree `muse open --vendor`
//! takes; a failed run bails with the stderr cargo wrote.

use std::path::Path;

use aether_bloomery_kinds::EncodedArtifact;
use aether_bloomery_workspace_programs::WORKSPACE_PROGRAMS;
use aether_bloomery_workspace_programs::vendor::{CargoVendor, VendorInput, VendorResult};
use aether_data::Ref;
use anyhow::{Context, Result, anyhow};
use clap::Args;

use super::EngineArgs;
use super::call::call_in;
use crate::bloomery::{load, parse_digest, read_each, read_value};
use crate::import_commit::Imported;

/// Arguments for `cargo xtask muse vendor`.
#[derive(Args, Debug)]
pub(super) struct VendorArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// The commit whose tracked files hold the `Cargo.lock` to vendor,
    /// imported as `import-commit` does; any revision `git rev-parse`
    /// resolves.
    #[arg(long, required_unless_present = "tree", conflicts_with = "tree")]
    commit: Option<String>,
    /// A source tree already in the journal, by its digest.
    #[arg(long)]
    tree: Option<String>,
    /// The environment cargo runs in, by its digest: the one the head
    /// `(aether.workspace.environment, <platform>)` names.
    #[arg(long)]
    environment: String,
}

/// Vendor the source and print `vendor=<digest>`, or fail with cargo's stderr.
pub(super) fn run(args: &VendorArgs) -> Result<()> {
    let environment = parse_digest(&args.environment).context("--environment")?;
    let mut engine = args.engine.connect()?;

    let source = match (&args.commit, &args.tree) {
        (Some(commit), _) => Imported::read(Path::new("."), commit)?.stage(&mut engine)?.root,
        (None, Some(tree)) => parse_digest(tree).context("--tree")?,
        (None, None) => return Err(anyhow!("name the source with --commit or --tree")),
    };

    let input = EncodedArtifact::new(&VendorInput {
        source: Ref::from_digest(source),
        environment: Ref::from_digest(environment),
    })?;
    let input_ref = Ref::from_digest(input.digest());
    engine.stage_artifacts(vec![input])?;

    let (_, result) = call_in::<CargoVendor>(&mut engine, WORKSPACE_PROGRAMS, input_ref)?;
    match read_value::<VendorResult>(&mut engine, result)? {
        VendorResult::Vendored { tree } => println!("vendor={}", tree.digest()),
        VendorResult::Failed { stderr } => {
            let digest = stderr.digest();
            let mut text = String::new();
            read_each(&mut engine, &[digest], |digest, artifact| {
                text = String::from_utf8_lossy(&load(artifact, digest)?).into_owned();
                Ok(())
            })?;
            return Err(anyhow!("vendor.cargo failed:\n{text}"));
        }
    }
    Ok(())
}
