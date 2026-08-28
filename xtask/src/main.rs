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
//! duplicate code, unused dependencies, and added suppressions;
//! `verify.member` is the same set less docs, which a single member's
//! closure can neither break nor prove on its own.

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
mod fixtures;
mod inventory;
mod package;
mod scope;
mod symbols;
mod transform;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::affected::AffectedArgs;
use crate::bloom::BloomArgs;
use crate::dev_component::DevComponentArgs;
use crate::dist::DistArgs;
use crate::fixtures::FixturesArgs;
use crate::package::PackageArgs;
use crate::scope::ScopeArgs;
use crate::symbols::SymbolsArgs;
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
    /// Rewrite or check pinned golden fixture files.
    Fixtures(FixturesArgs),
    /// Emit the shippable depot layout (ADR-0163 §1): the chassis binary,
    /// the workspace license files, a persisted `pack/manifest`, and
    /// content-addressed component objects under `pack/objects/<sha256>`.
    /// The Steam depot is this directory uploaded verbatim.
    Package(PackageArgs),
    /// ADR-0149 §Execution's portable execution unit: run one typed
    /// mechanical-verify command (`verify.fmt`, `verify.clippy`,
    /// `verify.docs`, `verify.test`, `verify.dup`, `verify.deps`, or
    /// `verify.suppress`) — the same invocation CI runs — and write
    /// nonce-tagged evidence bytes. `verify.check` runs the full set;
    /// `verify.member` runs it less `verify.docs`, which the member
    /// position leaves to the two whole-tree positions.
    Transform(TransformArgs),
    /// Compute the affected package set for PR CI test selection
    /// (issue #3611): changed paths against a base ref, mapped through
    /// the workspace graph's reverse-dependency closure.
    Affected(AffectedArgs),
    /// Drive the Bloomery coordinator REST surface: list blooms, seal a
    /// draft, or supersede a predecessor without composing JSON by hand.
    Bloom(BloomArgs),
    /// Workspace symbol inventory: build a deterministic table, find by
    /// name, or diff a working tree against a stored base table.
    Symbols(SymbolsArgs),
    /// Append one field write to a `scope.fill` run's call log. The value
    /// arrives by file so multi-paragraph prose survives the transport.
    Scope(ScopeArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::DevComponent(args) => dev_component::run(&args),
        Commands::Dist(args) => dist::run(&args),
        Commands::Fixtures(args) => fixtures::run(&args),
        Commands::Package(args) => package::run(&args),
        Commands::Transform(args) => transform::run(&args),
        Commands::Affected(args) => affected::run(&args),
        Commands::Bloom(args) => bloom::run(&args),
        Commands::Symbols(args) => symbols::run(&args),
        Commands::Scope(args) => scope::run(&args),
    }
}
