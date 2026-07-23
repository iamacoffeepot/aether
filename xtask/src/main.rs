//! Aether repo build tasks (`cargo xtask …`).
//!
//! `dist` packages the component wasm + chassis binaries into a stable
//! `dist/` tree with a typed `manifest.json`, so a harness running
//! outside a cargo-test process (no `CARGO_*` anchors) can locate every
//! artifact through the manifest. `dist/` is additive — the substrate
//! `target/` tree is still populated identically, so in-process scenario
//! tests (which read `target/…`) are untouched.
//!
//! `transform` is ADR-0149 §Execution's portable execution unit: it
//! runs one typed `verify.fmt` / `verify.clippy` / `verify.docs`
//! command — the same cargo invocation CI runs — identically on a
//! laptop and under the thin `transform.yml` wrapper workflow.
//! `verify.test` parity is a follow-up (issue #3501) — CI's actual
//! test lane is a heavier shape this slice doesn't reproduce.

// xtask is a developer-facing build tool: emitting build progress + a
// summary to the terminal is its purpose. The workspace
// `print_stdout = warn` lint targets actor / library code, where a stray
// print is a smell; here it is the intended output channel.
#![allow(clippy::print_stdout)]

mod affected;
mod cargo;
mod dist;
mod inventory;
mod package;
mod transform;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::affected::AffectedArgs;
use crate::dist::DistArgs;
use crate::package::PackageArgs;
use crate::transform::TransformArgs;

#[derive(Parser)]
#[command(name = "xtask", about = "Aether repo build tasks")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build component wasm + chassis bins into `dist/` with a manifest.
    Dist(DistArgs),
    /// Emit the shippable depot layout (ADR-0163 §1): the chassis binary,
    /// a persisted `pack/manifest`, and content-addressed component
    /// objects under `pack/objects/<sha256>`. The Steam depot is this
    /// directory uploaded verbatim.
    Package(PackageArgs),
    /// ADR-0149 §Execution's portable execution unit: run one typed
    /// mechanical-verify command (`verify.fmt` / `verify.clippy` /
    /// `verify.docs`) — the same cargo invocation CI runs — and write
    /// nonce-tagged evidence bytes. `verify.test` parity is a
    /// follow-up (issue #3501).
    Transform(TransformArgs),
    /// Compute the affected package set for PR CI test selection
    /// (issue #3611): changed paths against a base ref, mapped through
    /// the workspace graph's reverse-dependency closure.
    Affected(AffectedArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Dist(args) => dist::run(&args),
        Commands::Package(args) => package::run(&args),
        Commands::Transform(args) => transform::run(&args),
        Commands::Affected(args) => affected::run(&args),
    }
}
