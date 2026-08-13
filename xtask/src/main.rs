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
//! runs one typed `verify.*` command — the same invocation CI runs —
//! identically on a laptop and under the thin `transform.yml` wrapper
//! workflow. `verify.check` aggregates formatting, clippy, docs, tests,
//! duplicate code, unused dependencies, and added suppressions.

// xtask is a developer-facing build tool: emitting build progress + a
// summary to the terminal is its purpose. The workspace
// `print_stdout = warn` lint targets actor / library code, where a stray
// print is a smell; here it is the intended output channel. `print_stderr`
// rides the same reasoning for the diagnostic half of that channel — a lane
// reporting that its own prerequisite step failed has nowhere else to say so,
// and swallowing it would hide the reason a member's result is unreliable.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod affected;
mod bloom;
mod cargo;
mod dev_component;
mod dist;
mod inventory;
mod package;
mod transform;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::affected::AffectedArgs;
use crate::bloom::BloomArgs;
use crate::dev_component::DevComponentArgs;
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
    /// Watch, build, upload, and hot-reload one component on an explicit engine.
    DevComponent(DevComponentArgs),
    /// Build component wasm + chassis bins into `dist/` with a manifest.
    Dist(DistArgs),
    /// Emit the shippable depot layout (ADR-0163 §1): the chassis binary,
    /// a persisted `pack/manifest`, and content-addressed component
    /// objects under `pack/objects/<sha256>`. The Steam depot is this
    /// directory uploaded verbatim.
    Package(PackageArgs),
    /// ADR-0149 §Execution's portable execution unit: run one typed
    /// mechanical-verify command (`verify.fmt`, `verify.clippy`,
    /// `verify.docs`, `verify.test`, `verify.dup`, `verify.deps`, or
    /// `verify.suppress`) — the same invocation CI runs — and write
    /// nonce-tagged evidence bytes. `verify.check` runs the full set.
    Transform(TransformArgs),
    /// Compute the affected package set for PR CI test selection
    /// (issue #3611): changed paths against a base ref, mapped through
    /// the workspace graph's reverse-dependency closure.
    Affected(AffectedArgs),
    /// Drive the coordinator REST surface: list blooms, seal a draft, or
    /// supersede a wedged predecessor onto the observed head.
    Bloom(BloomArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::DevComponent(args) => dev_component::run(&args),
        Commands::Dist(args) => dist::run(&args),
        Commands::Package(args) => package::run(&args),
        Commands::Transform(args) => transform::run(&args),
        Commands::Affected(args) => affected::run(&args),
        Commands::Bloom(args) => bloom::run(&args),
    }
}
