//! Aether repo build tasks (`cargo xtask …`).
//!
//! `dist` packages the component wasm + chassis binaries into a stable
//! `dist/` tree with a typed `manifest.json`, so a harness running
//! outside a cargo-test process (no `CARGO_*` anchors) can locate every
//! artifact through the manifest. `dist/` is additive — the substrate
//! `target/` tree is still populated identically, so in-process scenario
//! tests (which read `target/…`) are untouched.

// xtask is a developer-facing build tool: emitting build progress + a
// summary to the terminal is its purpose. The workspace
// `print_stdout = warn` lint targets actor / library code, where a stray
// print is a smell; here it is the intended output channel.
#![allow(clippy::print_stdout)]

mod inventory;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use anyhow::{Context, Result, bail};
use cargo_metadata::MetadataCommand;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::inventory::{
    BuildPlan, CHASSIS_BINS, CHASSIS_PACKAGE, Component, build_plans, discover_components,
};

/// Wasm triple the components cross-build to.
const WASM_TARGET: &str = "wasm32-unknown-unknown";

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
    /// Build a standalone, hub-less game executable with the game component
    /// embedded at build time (#1518).
    Bundle(BundleArgs),
}

#[derive(Args)]
struct DistArgs {
    /// Cargo profile to build and package.
    #[arg(long, value_enum, default_value_t = Profile::Debug)]
    profile: Profile,
    /// Skip the chassis (host-target) binary build + copy. The preflight
    /// fast path uses this to stay wasm-only.
    #[arg(long)]
    no_bins: bool,
}

#[derive(Args)]
struct BundleArgs {
    /// Cargo profile for the game binary and its component.
    #[arg(long, value_enum, default_value_t = Profile::Release)]
    profile: Profile,
    /// Cross-compile the game binary for this target triple (e.g.
    /// `x86_64-pc-windows-msvc`). Defaults to the host target.
    #[arg(long)]
    target: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Profile {
    Debug,
    Release,
}

impl Profile {
    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }

    /// Cargo's profile flag — debug is the default (no flag).
    fn cargo_flag(self) -> Option<&'static str> {
        match self {
            Self::Debug => None,
            Self::Release => Some("--release"),
        }
    }
}

/// `dist/manifest.json` schema. Paths are relative to `dist/` and use
/// forward slashes so the manifest is stable across host OSes.
#[derive(Serialize)]
struct Manifest {
    /// Triple the component wasm is built for (`wasm32-unknown-unknown`).
    target: String,
    /// Cargo profile the tree was built under (`debug` / `release`).
    profile: String,
    /// Wasm stem → `components/<stem>.wasm`.
    components: BTreeMap<String, String>,
    /// Chassis bin name → `bin/<name>`. Empty under `--no-bins`.
    chassis: BTreeMap<String, String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Dist(args) => run_dist(&args),
        Commands::Bundle(args) => run_bundle(&args),
    }
}

fn run_dist(args: &DistArgs) -> Result<()> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("run cargo metadata")?;

    let components = discover_components(&metadata);
    if components.is_empty() {
        bail!("no wasm component crates discovered (cdylib target + aether-actor dep)");
    }

    let workspace_root = metadata.workspace_root.as_std_path();
    let target_dir = metadata.target_directory.as_std_path();
    let wasm_profile_dir = target_dir.join(WASM_TARGET).join(args.profile.as_str());
    let dist = workspace_root.join("dist");

    // Build each component package in its own cargo invocation — never
    // batch multiple `-p`. See `inventory::build_plans`.
    for plan in build_plans(&components) {
        build_component(&plan, args.profile)?;
    }
    if !args.no_bins {
        build_chassis(args.profile)?;
    }

    // Regenerate dist/ from scratch so the manifest is authoritative
    // (e.g. a prior `--no-bins`-then-full run can't leave stale state).
    if dist.exists() {
        fs::remove_dir_all(&dist).with_context(|| format!("clear {}", dist.display()))?;
    }
    fs::create_dir_all(dist.join("components")).context("create dist/components")?;

    let mut component_paths = BTreeMap::new();
    for component in &components {
        let src = wasm_artifact_path(&wasm_profile_dir, component);
        let rel = format!("components/{}.wasm", component.stem);
        copy_artifact(&src, &dist.join(&rel))?;
        component_paths.insert(component.stem.clone(), rel);
    }

    let mut chassis_paths = BTreeMap::new();
    if !args.no_bins {
        fs::create_dir_all(dist.join("bin")).context("create dist/bin")?;
        let host_profile_dir = target_dir.join(args.profile.as_str());
        for bin in CHASSIS_BINS {
            let src = host_profile_dir.join(bin);
            let rel = format!("bin/{bin}");
            copy_artifact(&src, &dist.join(&rel))?;
            chassis_paths.insert((*bin).to_string(), rel);
        }
    }

    let manifest = Manifest {
        target: WASM_TARGET.to_string(),
        profile: args.profile.as_str().to_string(),
        components: component_paths,
        chassis: chassis_paths,
    };
    let manifest_path = dist.join("manifest.json");
    let mut json = serde_json::to_string_pretty(&manifest).context("serialize manifest")?;
    json.push('\n');
    fs::write(&manifest_path, json)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    println!(
        "dist: {} component(s), {} chassis bin(s) -> {}",
        manifest.components.len(),
        manifest.chassis.len(),
        manifest_path.display(),
    );
    Ok(())
}

/// The game's component package, its wasm stem, and the standalone bin name.
const GAME_COMPONENT: &str = "aether-kit";
const GAME_COMPONENT_STEM: &str = "aether_kit";
const GAME_BIN: &str = "aether-game";

/// Build a standalone game executable: build the game component for wasm, embed
/// it into [`GAME_BIN`] via the bundle crate's `build.rs` (which reads
/// `AETHER_GAME_WASM`), and report the resulting binary.
fn run_bundle(args: &BundleArgs) -> Result<()> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("run cargo metadata")?;
    let target_dir = metadata.target_directory.as_std_path();

    // 1. Build the game component for wasm.
    let mut wasm_cmd = Command::new(cargo());
    wasm_cmd.args(["build", "--target", WASM_TARGET, "-p", GAME_COMPONENT]);
    if let Some(flag) = args.profile.cargo_flag() {
        wasm_cmd.arg(flag);
    }
    run(wasm_cmd, "build game component wasm")?;
    let wasm = target_dir
        .join(WASM_TARGET)
        .join(args.profile.as_str())
        .join(format!("{GAME_COMPONENT_STEM}.wasm"));
    if !wasm.exists() {
        bail!("game component wasm not found at {}", wasm.display());
    }

    // 2. Build the game binary with the wasm staged for `include_bytes!`.
    let mut bin_cmd = Command::new(cargo());
    bin_cmd.args(["build", "-p", CHASSIS_PACKAGE, "--bin", GAME_BIN]);
    if let Some(flag) = args.profile.cargo_flag() {
        bin_cmd.arg(flag);
    }
    if let Some(triple) = &args.target {
        bin_cmd.args(["--target", triple]);
    }
    bin_cmd.env("AETHER_GAME_WASM", &wasm);
    run(bin_cmd, "build game binary")?;

    // 3. Report the output path.
    let profile_dir = args.target.as_ref().map_or_else(
        || target_dir.join(args.profile.as_str()),
        |triple| target_dir.join(triple).join(args.profile.as_str()),
    );
    let windows = args
        .target
        .as_deref()
        .is_some_and(|t| t.contains("windows"));
    let exe = profile_dir.join(if windows {
        format!("{GAME_BIN}.exe")
    } else {
        GAME_BIN.to_string()
    });
    println!("game bundle -> {}", exe.display());
    Ok(())
}

/// Source path of a component's wasm under the target tree. Example
/// cdylibs land under `examples/`; lib cdylibs directly under the profile
/// dir.
fn wasm_artifact_path(wasm_profile_dir: &Path, component: &Component) -> PathBuf {
    let file = format!("{}.wasm", component.stem);
    if component.from_example {
        wasm_profile_dir.join("examples").join(file)
    } else {
        wasm_profile_dir.join(file)
    }
}

fn copy_artifact(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

fn build_component(plan: &BuildPlan, profile: Profile) -> Result<()> {
    let mut cmd = Command::new(cargo());
    cmd.args(["build", "--target", WASM_TARGET, "-p", &plan.package]);
    if plan.examples {
        cmd.arg("--examples");
    }
    if let Some(flag) = profile.cargo_flag() {
        cmd.arg(flag);
    }
    let label = if plan.examples {
        format!("{} (examples)", plan.package)
    } else {
        plan.package.clone()
    };
    run(cmd, &format!("build component {label}"))
}

fn build_chassis(profile: Profile) -> Result<()> {
    let mut cmd = Command::new(cargo());
    cmd.args(["build", "-p", CHASSIS_PACKAGE]);
    for bin in CHASSIS_BINS {
        cmd.args(["--bin", bin]);
    }
    if let Some(flag) = profile.cargo_flag() {
        cmd.arg(flag);
    }
    run(cmd, "build chassis bins")
}

fn run(mut cmd: Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("spawn cargo to {what}"))?;
    if !status.success() {
        bail!("cargo failed to {what} ({status})");
    }
    Ok(())
}

/// Cargo binary to re-invoke — honours the `CARGO` env var cargo sets for
/// subprocesses, falling back to `cargo` on `PATH`.
fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::inventory::discover_components;

    #[test]
    fn discovers_expected_component_set() {
        let metadata = cargo_metadata::MetadataCommand::new()
            .no_deps()
            .exec()
            .expect("run cargo metadata");
        let components = discover_components(&metadata);
        let stems: BTreeSet<&str> = components.iter().map(|c| c.stem.as_str()).collect();

        // Parity with the structural sweep preflight / CI ran before this
        // xtask: a drop here surfaces as an AETHER_REQUIRE_RUNTIME panic.
        for expected in [
            "probe",
            "probe_with_config",
            "multi_actor",
            "aether_camera",
            "aether_mesh_viewer",
            "aether_demo_sokoban",
        ] {
            assert!(
                stems.contains(expected),
                "discovery dropped component {expected}; found {stems:?}",
            );
        }

        // aether-actor's own example cdylibs are NOT components — the
        // crate does not depend on itself, so it fails the actor-dep gate.
        for excluded in ["hello", "echoer", "caller", "input_logger"] {
            assert!(
                !stems.contains(excluded),
                "discovery wrongly included aether-actor example {excluded}",
            );
        }
    }
}
