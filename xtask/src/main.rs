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
    /// Build a standalone, hub-less executable: a chassis with an ordered
    /// component set (plus configs) embedded at build time (#1529).
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
    /// Cargo profile for the bundle binary and its components.
    #[arg(long, value_enum, default_value_t = Profile::Release)]
    profile: Profile,
    /// Cross-compile the bundle binary for this target triple (e.g.
    /// `x86_64-pc-windows-msvc`). Defaults to the host target.
    #[arg(long)]
    target: Option<String>,
    /// Chassis the bundle boots.
    #[arg(long, value_enum, default_value_t = BundleChassis::Desktop)]
    chassis: BundleChassis,
    /// Ordered components to embed (autoload order is argument order):
    /// a workspace package name, built for wasm32 as its lib cdylib, or
    /// a path to a prebuilt artifact (recognized by the `.wasm` suffix
    /// — use this for `[[example]]` cdylibs).
    #[arg(long, num_args = 1.., required_unless_present = "spec")]
    components: Vec<String>,
    /// Per-component init-config file (ADR-0090), paired with
    /// `--components` by position (repeat the flag; trailing components
    /// without a config get empty config bytes).
    #[arg(long = "config")]
    configs: Vec<PathBuf>,
    /// Window title (desktop chassis only).
    #[arg(long)]
    title: Option<String>,
    /// Window mode spec (desktop chassis only), same vocabulary as
    /// `AETHER_WINDOW_MODE` (`windowed[:WxH]` / `fullscreen-borderless`
    /// / `exclusive:WxH@HZ`).
    #[arg(long)]
    window_mode: Option<String>,
    /// Tick cadence in hertz (headless chassis only).
    #[arg(long)]
    tick_hz: Option<u32>,
    /// Full-fidelity bundle spec (JSON) — alternative to the component
    /// and chassis-config flags. Carries chassis, `title` /
    /// `window_mode` / `tick_hz`, and per-component `package`-or-`wasm` + `config` +
    /// `name` + `export`; relative paths resolve against the spec
    /// file's directory.
    #[arg(
        long,
        conflicts_with_all = ["components", "configs", "title", "window_mode", "tick_hz"]
    )]
    spec: Option<PathBuf>,
}

/// Which chassis a bundle boots. Each maps to a generic bundle bin in
/// the chassis package; the two are distinct binaries because the
/// chassis are genuinely different link sets (desktop pulls
/// winit/wgpu/cpal, headless none).
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum BundleChassis {
    Desktop,
    Headless,
}

impl BundleChassis {
    fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Headless => "headless",
        }
    }

    /// The generic bundle bin (`[[bin]]` in the chassis package) that
    /// embeds the pack for this chassis.
    fn bin_name(self) -> &'static str {
        match self {
            Self::Desktop => "aether-bundle-desktop",
            Self::Headless => "aether-bundle-headless",
        }
    }
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

/// One component in a resolved bundle plan: where its wasm comes from
/// plus the per-component load inputs that ride into the pack manifest.
struct PlannedComponent {
    source: ComponentSource,
    config: Option<PathBuf>,
    name: Option<String>,
    export: Option<String>,
}

/// Where a planned component's wasm comes from.
enum ComponentSource {
    /// A workspace package whose lib cdylib xtask builds for wasm32.
    Package(String),
    /// A prebuilt `.wasm` artifact supplied by path.
    Prebuilt(PathBuf),
}

/// The normalized bundle inputs — flags and the `--spec` file both
/// resolve to this before any cargo invocation runs.
struct BundlePlan {
    chassis: BundleChassis,
    title: Option<String>,
    window_mode: Option<String>,
    tick_hz: Option<u32>,
    components: Vec<PlannedComponent>,
}

/// `--spec` file schema (JSON). Mirrors [`BundlePlan`] with
/// per-component `package` XOR `wasm`.
#[derive(serde::Deserialize)]
struct BundleSpec {
    /// Overrides the `--chassis` flag when present.
    #[serde(default)]
    chassis: Option<BundleChassis>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    window_mode: Option<String>,
    #[serde(default)]
    tick_hz: Option<u32>,
    components: Vec<SpecComponent>,
}

/// One component entry in a [`BundleSpec`].
#[derive(serde::Deserialize)]
struct SpecComponent {
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    wasm: Option<PathBuf>,
    #[serde(default)]
    config: Option<PathBuf>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    export: Option<String>,
}

/// Build a standalone, hub-less executable (#1529): build each listed
/// component for wasm32, write the pack manifest, and build the
/// chassis's generic bundle bin with `AETHER_BUNDLE_MANIFEST` pointing
/// at it (the chassis package's `build.rs` packs the wasms for
/// `include_bytes!`). Reports the resulting binary.
fn run_bundle(args: &BundleArgs) -> Result<()> {
    let plan = resolve_bundle_plan(args)?;
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("run cargo metadata")?;
    let target_dir = metadata.target_directory.as_std_path();

    // 1. Build (or locate) each component's wasm, in order. One cargo
    // invocation per package — never batch multiple `-p` (see
    // `inventory::build_plans` on the feature-unification trap).
    let mut wasm_paths = Vec::new();
    for component in &plan.components {
        let wasm = match &component.source {
            ComponentSource::Package(package) => {
                let mut wasm_cmd = Command::new(cargo());
                wasm_cmd.args(["build", "--target", WASM_TARGET, "-p", package]);
                if let Some(flag) = args.profile.cargo_flag() {
                    wasm_cmd.arg(flag);
                }
                run(wasm_cmd, &format!("build component wasm for {package}"))?;
                let stem = package.replace('-', "_");
                let wasm = target_dir
                    .join(WASM_TARGET)
                    .join(args.profile.as_str())
                    .join(format!("{stem}.wasm"));
                if !wasm.exists() {
                    bail!(
                        "component wasm for {package} not found at {} \
                         (packages bundle their lib cdylib; pass a prebuilt \
                         .wasm path for [[example]] cdylibs)",
                        wasm.display(),
                    );
                }
                wasm
            }
            ComponentSource::Prebuilt(path) => fs::canonicalize(path)
                .with_context(|| format!("locate prebuilt component wasm {}", path.display()))?,
        };
        wasm_paths.push(wasm);
    }

    // 2. Write the pack manifest the chassis package's `build.rs` reads.
    let manifest = bundle_manifest_json(&plan, &wasm_paths)?;
    let manifest_dir = target_dir.join("bundle");
    fs::create_dir_all(&manifest_dir)
        .with_context(|| format!("create {}", manifest_dir.display()))?;
    let manifest_path =
        manifest_dir.join(format!("{}-bundle-manifest.json", plan.chassis.as_str()));
    let mut manifest_text =
        serde_json::to_string_pretty(&manifest).context("serialize bundle manifest")?;
    manifest_text.push('\n');
    fs::write(&manifest_path, manifest_text)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    // 3. Build the chassis's generic bundle bin with the pack staged
    // for `include_bytes!`.
    let bin = plan.chassis.bin_name();
    let mut bin_cmd = Command::new(cargo());
    bin_cmd.args(["build", "-p", CHASSIS_PACKAGE, "--bin", bin]);
    if let Some(flag) = args.profile.cargo_flag() {
        bin_cmd.arg(flag);
    }
    if let Some(triple) = &args.target {
        bin_cmd.args(["--target", triple]);
    }
    bin_cmd.env("AETHER_BUNDLE_MANIFEST", &manifest_path);
    run(bin_cmd, &format!("build bundle binary {bin}"))?;

    // 4. Report the output path.
    let profile_dir = args.target.as_ref().map_or_else(
        || target_dir.join(args.profile.as_str()),
        |triple| target_dir.join(triple).join(args.profile.as_str()),
    );
    let windows = args
        .target
        .as_deref()
        .map_or(cfg!(windows), |t| t.contains("windows"));
    let exe = profile_dir.join(if windows {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    });
    println!(
        "{} bundle ({} component(s)) -> {}",
        plan.chassis.as_str(),
        plan.components.len(),
        exe.display(),
    );
    Ok(())
}

/// Normalize the bundle inputs: `--spec <file>` when present, the
/// component + chassis-config flags otherwise.
fn resolve_bundle_plan(args: &BundleArgs) -> Result<BundlePlan> {
    if let Some(spec_path) = &args.spec {
        return resolve_bundle_spec(spec_path, args.chassis);
    }
    if args.configs.len() > args.components.len() {
        bail!(
            "{} --config values for {} --components entries — configs pair by position",
            args.configs.len(),
            args.components.len(),
        );
    }
    let components = args
        .components
        .iter()
        .enumerate()
        .map(|(i, raw)| PlannedComponent {
            source: classify_component(raw),
            config: args.configs.get(i).cloned(),
            name: None,
            export: None,
        })
        .collect();
    Ok(BundlePlan {
        chassis: args.chassis,
        title: args.title.clone(),
        window_mode: args.window_mode.clone(),
        tick_hz: args.tick_hz,
        components,
    })
}

/// Parse a `--spec` file into a plan. Relative paths inside the spec
/// resolve against the spec file's directory.
fn resolve_bundle_spec(spec_path: &Path, chassis_flag: BundleChassis) -> Result<BundlePlan> {
    let text = fs::read_to_string(spec_path)
        .with_context(|| format!("read bundle spec {}", spec_path.display()))?;
    let spec: BundleSpec = serde_json::from_str(&text)
        .with_context(|| format!("parse bundle spec {}", spec_path.display()))?;
    let spec_dir = spec_path.parent().unwrap_or_else(|| Path::new("."));
    let anchor = |path: &Path| -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            spec_dir.join(path)
        }
    };
    let mut components = Vec::new();
    for (i, entry) in spec.components.iter().enumerate() {
        let source = match (&entry.package, &entry.wasm) {
            (Some(package), None) => ComponentSource::Package(package.clone()),
            (None, Some(wasm)) => ComponentSource::Prebuilt(anchor(wasm)),
            _ => {
                bail!("bundle spec component {i}: exactly one of `package` or `wasm` is required")
            }
        };
        components.push(PlannedComponent {
            source,
            config: entry.config.as_deref().map(anchor),
            name: entry.name.clone(),
            export: entry.export.clone(),
        });
    }
    if components.is_empty() {
        bail!("bundle spec {} lists no components", spec_path.display());
    }
    Ok(BundlePlan {
        chassis: spec.chassis.unwrap_or(chassis_flag),
        title: spec.title,
        window_mode: spec.window_mode,
        tick_hz: spec.tick_hz,
        components,
    })
}

/// A `--components` entry is a prebuilt artifact iff it carries the
/// `.wasm` suffix; anything else is a workspace package name.
fn classify_component(raw: &str) -> ComponentSource {
    let is_wasm_path = Path::new(raw)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"));
    if is_wasm_path {
        ComponentSource::Prebuilt(PathBuf::from(raw))
    } else {
        ComponentSource::Package(raw.to_string())
    }
}

/// Render the pack manifest JSON the chassis package's `build.rs`
/// consumes (`BundleManifest` in
/// `crates/aether-substrate-bundle/src/bundle_pack.rs` — xtask doesn't
/// depend on the chassis crate, so keep the field names in sync).
/// Component order is plan order; config paths are canonicalized here
/// so the manifest carries only absolute paths.
fn bundle_manifest_json(plan: &BundlePlan, wasm_paths: &[PathBuf]) -> Result<serde_json::Value> {
    let mut components = Vec::new();
    for (component, wasm) in plan.components.iter().zip(wasm_paths) {
        let config = component
            .config
            .as_ref()
            .map(|path| {
                fs::canonicalize(path)
                    .with_context(|| format!("locate component config {}", path.display()))
            })
            .transpose()?;
        components.push(serde_json::json!({
            "wasm": wasm,
            "config": config,
            "name": component.name,
            "export": component.export,
        }));
    }
    Ok(serde_json::json!({
        "chassis": plan.chassis.as_str(),
        "title": plan.title,
        "window_mode": plan.window_mode,
        "tick_hz": plan.tick_hz,
        "components": components,
    }))
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
    use std::path::PathBuf;

    use super::inventory::discover_components;
    use super::{
        BundleChassis, BundlePlan, ComponentSource, PlannedComponent, bundle_manifest_json,
    };

    #[test]
    fn bundle_manifest_carries_chassis_and_component_order() {
        // The manifest is the contract between xtask and the chassis
        // package's `build.rs`: chassis string, chassis settings, and
        // the component list in plan (= autoload) order.
        let plan = BundlePlan {
            chassis: BundleChassis::Headless,
            title: None,
            window_mode: None,
            tick_hz: Some(30),
            components: vec![
                PlannedComponent {
                    source: ComponentSource::Prebuilt(PathBuf::from("/abs/first.wasm")),
                    config: None,
                    name: Some("first".to_owned()),
                    export: None,
                },
                PlannedComponent {
                    source: ComponentSource::Prebuilt(PathBuf::from("/abs/second.wasm")),
                    config: None,
                    name: None,
                    export: Some("alt".to_owned()),
                },
            ],
        };
        let wasm_paths = vec![
            PathBuf::from("/abs/first.wasm"),
            PathBuf::from("/abs/second.wasm"),
        ];
        let manifest = bundle_manifest_json(&plan, &wasm_paths).expect("render manifest");
        assert_eq!(manifest["chassis"], "headless");
        assert_eq!(manifest["tick_hz"], 30);
        assert_eq!(manifest["title"], serde_json::Value::Null);
        let components = manifest["components"].as_array().expect("components array");
        assert_eq!(components.len(), 2);
        assert_eq!(components[0]["wasm"], "/abs/first.wasm");
        assert_eq!(components[0]["name"], "first");
        assert_eq!(components[1]["wasm"], "/abs/second.wasm");
        assert_eq!(components[1]["export"], "alt");
    }

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
