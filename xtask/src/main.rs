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
mod inventory;
mod transform;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use aether_chassis::bundle_pack::ChassisSettings;
use aether_chassis::package::{PackageEntry, PackageManifest, Sha256, encode_manifest};
use anyhow::{Context, Result, bail};
use cargo_metadata::{Metadata, MetadataCommand};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::affected::AffectedArgs;
use crate::inventory::{
    BUNDLE_PACKAGE, BuildPlan, CHASSIS_BINS, Component, PACKAGE_CHASSIS, PACKAGE_CHASSIS_HEADLESS,
    behavior_build_plans, build_plans, discover_behavior_variants, discover_behaviors, discover_components,
};
use crate::transform::TransformArgs;

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

#[derive(Args)]
struct DistArgs {
    /// Cargo profile to build and package.
    #[arg(long, value_enum, default_value_t = Profile::Debug)]
    profile: Profile,
    /// Skip the chassis (host-target) binary build + copy — a wasm-only fast
    /// path for callers that only need the component wasm.
    #[arg(long)]
    no_bins: bool,
}

#[derive(Args)]
struct PackageArgs {
    /// Cargo profile to build and package. A depot ships release
    /// artifacts, so the package target defaults to release (unlike
    /// `dist`, whose consumers are test harnesses).
    #[arg(long, value_enum, default_value_t = Profile::Release)]
    profile: Profile,
    /// Output directory for the depot layout. Defaults to
    /// `target/package/`. The directory is regenerated from scratch each
    /// run so the manifest stays authoritative.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Chassis the depot ships. Selects the `(package, bin)` pair from
    /// the chassis inventory; `headless` ships the headless substrate.
    #[arg(long, value_enum, default_value_t = BundleChassis::Desktop)]
    chassis: BundleChassis,
    /// Ordered components to select (autoload order is argument order):
    /// a workspace package name, built for wasm32 as its lib cdylib, or
    /// a path to a prebuilt artifact (recognized by the `.wasm` suffix
    /// — use this for `[[example]]` cdylibs). Omit both this and `--spec`
    /// for the discover-everything dev sweep (desktop chassis, default
    /// settings).
    #[arg(long, num_args = 1..)]
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
    /// Full-fidelity depot spec (JSON) — alternative to the component
    /// and chassis-config flags. Carries chassis, `title` /
    /// `window_mode` / `tick_hz`, and per-component `package`-or-`wasm` +
    /// `config` + `name` + `export`; relative paths resolve against the
    /// spec file's directory.
    #[arg(
        long,
        conflicts_with_all = ["components", "configs", "title", "window_mode", "tick_hz"]
    )]
    spec: Option<PathBuf>,
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

    /// The chassis substrate `(package, bin)` pair a `cargo xtask package`
    /// depot ships for this chassis — the real host binary from the chassis
    /// inventory (not the standalone-bundle bin [`bin_name`] returns).
    fn package_chassis(self) -> (&'static str, &'static str) {
        match self {
            Self::Desktop => PACKAGE_CHASSIS,
            Self::Headless => PACKAGE_CHASSIS_HEADLESS,
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
        Commands::Package(args) => run_package(&args),
        Commands::Transform(args) => transform::run(&args),
        Commands::Affected(args) => affected::run(&args),
    }
}

fn run_dist(args: &DistArgs) -> Result<()> {
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;

    let components = discover_components(&metadata);
    if components.is_empty() {
        bail!("no wasm component crates discovered (cdylib target + aether-actor dep)");
    }
    // Behavior-script fixtures (ADR-0137, issue 2688): cross-built alongside
    // the components so the in-process scenario tests locate their wasm under
    // `target/.../examples/`. They are not components (no `aether-actor`), so
    // they ride their own discovery pass and are not copied into the dist
    // component manifest — only built.
    let behaviors = discover_behaviors(&metadata);

    let workspace_root = metadata.workspace_root.as_std_path();
    let target_dir = metadata.target_directory.as_std_path();
    let wasm_profile_dir = target_dir.join(WASM_TARGET).join(args.profile.as_str());
    let dist = workspace_root.join("dist");

    // Build host-carrying variants FIRST (issue 2688): the feature build
    // clobbers `<stem>.wasm`, so we copy it to `<stem>_behavior.wasm` and then
    // let the stock component loop below rebuild `<stem>.wasm` lean. Only the
    // behavior-host scenario loads the `_behavior` stem; every other kit
    // consumer keeps the small stock wasm.
    for variant in discover_behavior_variants(&metadata) {
        let plan = BuildPlan { package: variant.package.clone(), examples: false, features: variant.features.clone() };
        build_component(&plan, args.profile)?;
        let built = wasm_profile_dir.join(format!("{}.wasm", variant.stem));
        let variant_stem = wasm_profile_dir.join(format!("{}_behavior.wasm", variant.stem));
        fs::copy(&built, &variant_stem)
            .with_context(|| format!("copy {} -> {}", built.display(), variant_stem.display()))?;
    }

    // Build each component package in its own cargo invocation — never
    // batch multiple `-p`. See `inventory::build_plans`.
    for plan in build_plans(&components) {
        build_component(&plan, args.profile)?;
    }
    for plan in behavior_build_plans(&behaviors) {
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
        for (_, bin) in CHASSIS_BINS {
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
    fs::write(&manifest_path, json).with_context(|| format!("write {}", manifest_path.display()))?;

    println!(
        "dist: {} component(s), {} chassis bin(s) -> {}",
        manifest.components.len(),
        manifest.chassis.len(),
        manifest_path.display(),
    );
    if !behaviors.is_empty() {
        let stems: Vec<&str> = behaviors.iter().map(|b| b.stem.as_str()).collect();
        println!("dist: {} behavior script(s) built into target/: {}", stems.len(), stems.join(", "));
    }
    Ok(())
}

/// Emit the shippable depot layout (ADR-0163 §1): the chassis binary, a
/// persisted `pack/manifest`, and content-addressed component objects.
///
/// ```text
/// <out>/
///   aether-substrate            # chassis binary (desktop or headless)
///   pack/manifest               # `encode_manifest` output
///   pack/objects/<sha256>       # component wasm (+ config), content-addressed
/// ```
///
/// Two input surfaces resolve to the same emit (ported from `cargo xtask
/// bundle`, issue #4002):
///
/// - **No selection** — the discover-everything dev sweep: every
///   structurally discovered component, the desktop chassis, default
///   [`ChassisSettings`]. `name` labels mirror `dist` (the wasm stems).
/// - **`--components` / `--spec`** — a real product: the chosen chassis
///   binary plus only the selected components, with per-component
///   `config` / `name` / `export` and the chassis `title` / `window_mode`
///   / `tick_hz` riding into `pack/manifest`.
///
/// Each object is referenced from the manifest by its sha256 hash, so
/// identity is the content and a name is a label.
fn run_package(args: &PackageArgs) -> Result<()> {
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;
    let target_dir = metadata.target_directory.as_std_path();
    let out = args.out.clone().unwrap_or_else(|| target_dir.join("package"));

    // A `--spec` file or an explicit `--components` set makes this a product
    // emit; with neither it is the discover-everything sweep.
    let selected = args.spec.is_some() || !args.components.is_empty();
    let (chassis_bin, components, settings) = if selected {
        let plan = resolve_bundle_plan(
            args.spec.as_deref(),
            args.chassis,
            &args.components,
            &args.configs,
            args.title.as_deref(),
            args.window_mode.as_deref(),
            args.tick_hz,
        )?;
        let (chassis_package, chassis_bin) = plan.chassis.package_chassis();
        let components = build_planned_components(&plan, target_dir, args.profile)?;
        build_named_chassis(chassis_package, chassis_bin, args.profile)?;
        let settings =
            ChassisSettings { title: plan.title.clone(), window_mode: plan.window_mode.clone(), tick_hz: plan.tick_hz };
        (chassis_bin, components, settings)
    } else {
        let (chassis_package, chassis_bin) = PACKAGE_CHASSIS;
        let components = sweep_components(&metadata, target_dir, args.profile)?;
        build_named_chassis(chassis_package, chassis_bin, args.profile)?;
        (chassis_bin, components, ChassisSettings::default())
    };

    let chassis_src = target_dir.join(args.profile.as_str()).join(chassis_bin);
    let manifest = emit_depot(&out, &chassis_src, chassis_bin, &components, settings)?;

    println!(
        "package: {} component object(s) + {} chassis bin -> {}",
        manifest.entries.len(),
        chassis_bin,
        out.display(),
    );
    Ok(())
}

/// The discover-everything dev sweep component set: build every
/// structurally discovered component and read its wasm into a name-labelled
/// [`PackComponent`]. Stem-sorted so a rebuild of the same sources yields a
/// byte-identical `pack/manifest`; each package builds in its own cargo
/// invocation (never batch multiple `-p`, see `inventory::build_plans`).
fn sweep_components(metadata: &Metadata, target_dir: &Path, profile: Profile) -> Result<Vec<PackComponent>> {
    let mut components = discover_components(metadata);
    if components.is_empty() {
        bail!("no wasm component crates discovered (cdylib target + aether-actor dep)");
    }
    components.sort_by(|a, b| a.stem.cmp(&b.stem));

    for plan in build_plans(&components) {
        build_component(&plan, profile)?;
    }
    let wasm_profile_dir = target_dir.join(WASM_TARGET).join(profile.as_str());
    components
        .iter()
        .map(|component| {
            let src = wasm_artifact_path(&wasm_profile_dir, component);
            let wasm = fs::read(&src).with_context(|| format!("read component wasm {}", src.display()))?;
            Ok(PackComponent { wasm, config: None, name: Some(component.stem.clone()), export: None, replicas: None })
        })
        .collect()
}

/// Write the depot tree at `out`: copy the chassis binary to `<out>/<chassis_bin>`,
/// then write the `pack/` tree (content-addressed objects + `pack/manifest`)
/// via [`write_pack`]. Regenerates `out` from scratch so a stale prior run
/// can't leave orphaned objects. Returns the manifest it wrote.
fn emit_depot(
    out: &Path,
    chassis_src: &Path,
    chassis_bin: &str,
    components: &[PackComponent],
    settings: ChassisSettings,
) -> Result<PackageManifest> {
    if out.exists() {
        fs::remove_dir_all(out).with_context(|| format!("clear {}", out.display()))?;
    }
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    copy_artifact(chassis_src, &out.join(chassis_bin))?;
    write_pack(out, components, settings)
}

/// One component about to be written into a pack: its load labels plus the
/// wasm (and optional config) bytes that become content-addressed objects.
struct PackComponent {
    wasm: Vec<u8>,
    config: Option<Vec<u8>>,
    name: Option<String>,
    export: Option<String>,
    replicas: Option<u32>,
}

/// Write the `pack/` tree under `<root>/pack`: hash each component's wasm (and
/// optional config) into `pack/objects/<sha256>` and write the
/// [`encode_manifest`] bytes to `pack/manifest`. The `pack/` subtree is
/// regenerated from scratch so a stale prior run can't leave orphaned objects.
/// Shared by the depot [`emit_depot`] (which also copies the chassis binary
/// alongside) and the standalone-bundle build (which embeds the objects into
/// the bundle bin instead of shipping the `pack/` dir). Returns the manifest.
fn write_pack(root: &Path, components: &[PackComponent], settings: ChassisSettings) -> Result<PackageManifest> {
    let pack_dir = root.join("pack");
    if pack_dir.exists() {
        fs::remove_dir_all(&pack_dir).with_context(|| format!("clear {}", pack_dir.display()))?;
    }
    let objects_dir = pack_dir.join("objects");
    fs::create_dir_all(&objects_dir).with_context(|| format!("create {}", objects_dir.display()))?;

    let mut entries = Vec::with_capacity(components.len());
    for component in components {
        let object = write_object(&objects_dir, &component.wasm)?;
        let config = match &component.config {
            Some(bytes) => Some(write_object(&objects_dir, bytes)?),
            None => None,
        };
        entries.push(PackageEntry {
            object,
            config,
            name: component.name.clone(),
            export: component.export.clone(),
            replicas: component.replicas,
        });
    }

    let manifest = PackageManifest { settings, entries };
    let manifest_path = pack_dir.join("manifest");
    fs::write(&manifest_path, encode_manifest(&manifest))
        .with_context(|| format!("write {}", manifest_path.display()))?;
    Ok(manifest)
}

/// Hash `bytes` and write them to `<objects_dir>/<lowercase-hex>`, the
/// content-addressed object name the manifest references and the chassis
/// resolves against. Objects are immutable and content-keyed, so an
/// already-present object (a second component with identical bytes) is not
/// rewritten. Returns the [`Sha256`] identity.
fn write_object(objects_dir: &Path, bytes: &[u8]) -> Result<Sha256> {
    let mut hasher = Sha256Hasher::new();
    hasher.update(bytes);
    let object = Sha256(hasher.finalize().into());
    let path = objects_dir.join(object.to_hex());
    if !path.exists() {
        fs::write(&path, bytes).with_context(|| format!("write object {}", path.display()))?;
    }
    Ok(object)
}

/// Build one chassis binary by `(package, bin)` selector for the host
/// target — the package target's single-bin twin of `build_chassis`'s
/// all-bins build.
fn build_named_chassis(package: &str, bin: &str, profile: Profile) -> Result<()> {
    let mut cmd = Command::new(cargo());
    cmd.args(["build", "-p", package, "--bin", bin]);
    if let Some(flag) = profile.cargo_flag() {
        cmd.arg(flag);
    }
    run(cmd, &format!("build chassis bin {bin}"))
}

/// One component in a resolved bundle plan: where its wasm comes from
/// plus the per-component load inputs that ride into the pack manifest.
#[derive(Debug)]
struct PlannedComponent {
    source: ComponentSource,
    config: Option<PathBuf>,
    name: Option<String>,
    export: Option<String>,
}

/// Where a planned component's wasm comes from.
#[derive(Debug)]
enum ComponentSource {
    /// A workspace package whose lib cdylib xtask builds for wasm32.
    Package(String),
    /// A prebuilt `.wasm` artifact supplied by path.
    Prebuilt(PathBuf),
}

/// The normalized bundle inputs — flags and the `--spec` file both
/// resolve to this before any cargo invocation runs.
#[derive(Debug)]
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
/// component for wasm32, emit the depot-shaped package artifact (a
/// `pack/manifest` plus content-addressed objects), and build the chassis's
/// generic bundle bin with `AETHER_BUNDLE_PACK` pointing at it (the chassis
/// package's `build.rs` embeds the objects via `include_bytes!`, ADR-0163 §1).
/// Reports the resulting binary.
fn run_bundle(args: &BundleArgs) -> Result<()> {
    let plan = resolve_bundle_plan(
        args.spec.as_deref(),
        args.chassis,
        &args.components,
        &args.configs,
        args.title.as_deref(),
        args.window_mode.as_deref(),
        args.tick_hz,
    )?;
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;
    let target_dir = metadata.target_directory.as_std_path();

    // 1. Build (or locate) each component's wasm, in order, plus its config.
    let components = build_planned_components(&plan, target_dir, args.profile)?;

    // 2. Emit the depot-shaped pack the chassis package's `build.rs` embeds.
    let settings =
        ChassisSettings { title: plan.title.clone(), window_mode: plan.window_mode.clone(), tick_hz: plan.tick_hz };
    let pack_root = target_dir.join("bundle").join(format!("{}-pack", plan.chassis.as_str()));
    write_pack(&pack_root, &components, settings)?;

    // 3. Build the chassis's generic bundle bin with the pack staged for embed.
    let bin = plan.chassis.bin_name();
    let mut bin_cmd = Command::new(cargo());
    bin_cmd.args(["build", "-p", BUNDLE_PACKAGE, "--bin", bin]);
    if let Some(flag) = args.profile.cargo_flag() {
        bin_cmd.arg(flag);
    }
    if let Some(triple) = &args.target {
        bin_cmd.args(["--target", triple]);
    }
    bin_cmd.env("AETHER_BUNDLE_PACK", &pack_root);
    run(bin_cmd, &format!("build bundle binary {bin}"))?;

    // 4. Report the output path.
    let profile_dir = args.target.as_ref().map_or_else(
        || target_dir.join(args.profile.as_str()),
        |triple| target_dir.join(triple).join(args.profile.as_str()),
    );
    let windows = args.target.as_deref().map_or(cfg!(windows), |t| t.contains("windows"));
    let exe = profile_dir.join(if windows {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    });
    println!("{} bundle ({} component(s)) -> {}", plan.chassis.as_str(), plan.components.len(), exe.display());
    Ok(())
}

/// Build (or locate) each planned component's wasm, in plan order, and read
/// its bytes plus any per-component config bytes into a [`PackComponent`].
/// One cargo invocation per package — never batch multiple `-p` (see
/// `inventory::build_plans` on the feature-unification trap). Shared by
/// `bundle` and `package`; survives the eventual `bundle` retirement.
fn build_planned_components(plan: &BundlePlan, target_dir: &Path, profile: Profile) -> Result<Vec<PackComponent>> {
    let mut components = Vec::new();
    for component in &plan.components {
        let wasm_path = match &component.source {
            ComponentSource::Package(package) => {
                let mut wasm_cmd = Command::new(cargo());
                wasm_cmd.args(["build", "--target", WASM_TARGET, "-p", package]);
                if let Some(flag) = profile.cargo_flag() {
                    wasm_cmd.arg(flag);
                }
                run(wasm_cmd, &format!("build component wasm for {package}"))?;
                let stem = package.replace('-', "_");
                let wasm = target_dir.join(WASM_TARGET).join(profile.as_str()).join(format!("{stem}.wasm"));
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
            ComponentSource::Prebuilt(path) => {
                fs::canonicalize(path).with_context(|| format!("locate prebuilt component wasm {}", path.display()))?
            }
        };
        let wasm = fs::read(&wasm_path).with_context(|| format!("read component wasm {}", wasm_path.display()))?;
        let config = match &component.config {
            Some(path) => Some(fs::read(path).with_context(|| format!("read component config {}", path.display()))?),
            None => None,
        };
        components.push(PackComponent {
            wasm,
            config,
            name: component.name.clone(),
            export: component.export.clone(),
            replicas: None,
        });
    }
    Ok(components)
}

/// Normalize the pack inputs shared by `bundle` and `package`: `--spec
/// <file>` when present, the component + chassis-config flags otherwise.
/// Taking the input fields rather than a subcommand's args struct lets both
/// subcommands author identical packs, and survives the eventual `bundle`
/// retirement with `package`.
fn resolve_bundle_plan(
    spec: Option<&Path>,
    chassis: BundleChassis,
    components: &[String],
    configs: &[PathBuf],
    title: Option<&str>,
    window_mode: Option<&str>,
    tick_hz: Option<u32>,
) -> Result<BundlePlan> {
    if let Some(spec_path) = spec {
        return resolve_bundle_spec(spec_path, chassis);
    }
    if configs.len() > components.len() {
        bail!(
            "{} --config values for {} --components entries — configs pair by position",
            configs.len(),
            components.len(),
        );
    }
    let components = components
        .iter()
        .enumerate()
        .map(|(i, raw)| PlannedComponent {
            source: classify_component(raw),
            config: configs.get(i).cloned(),
            name: None,
            export: None,
        })
        .collect();
    Ok(BundlePlan {
        chassis,
        title: title.map(str::to_owned),
        window_mode: window_mode.map(str::to_owned),
        tick_hz,
        components,
    })
}

/// Parse a `--spec` file into a plan. Relative paths inside the spec
/// resolve against the spec file's directory.
fn resolve_bundle_spec(spec_path: &Path, chassis_flag: BundleChassis) -> Result<BundlePlan> {
    let text = fs::read_to_string(spec_path).with_context(|| format!("read bundle spec {}", spec_path.display()))?;
    let spec: BundleSpec =
        serde_json::from_str(&text).with_context(|| format!("parse bundle spec {}", spec_path.display()))?;
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
    let is_wasm_path = Path::new(raw).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"));
    if is_wasm_path {
        ComponentSource::Prebuilt(PathBuf::from(raw))
    } else {
        ComponentSource::Package(raw.to_string())
    }
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
    if !plan.features.is_empty() {
        cmd.args(["--features", &plan.features.join(",")]);
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
    // One invocation selects every owning package plus every bin —
    // bin selectors are global across the selected packages, and the
    // names are unique workspace-wide.
    cmd.arg("build");
    let mut packages: Vec<&str> = CHASSIS_BINS.iter().map(|(pkg, _)| *pkg).collect();
    packages.dedup();
    for pkg in packages {
        cmd.args(["-p", pkg]);
    }
    for (_, bin) in CHASSIS_BINS {
        cmd.args(["--bin", bin]);
    }
    if let Some(flag) = profile.cargo_flag() {
        cmd.arg(flag);
    }
    run(cmd, "build chassis bins")
}

fn run(mut cmd: Command, what: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawn cargo to {what}"))?;
    if !status.success() {
        bail!("cargo failed to {what} ({status})");
    }
    Ok(())
}

/// Cargo binary to re-invoke — honours the `CARGO` env var cargo sets for
/// subprocesses, falling back to `cargo` on `PATH`.
// Build tooling: CARGO is the cargo-provided binary path for subprocess
// re-invocation, an external var — xtask is not a capability.
#[allow(clippy::disallowed_methods)]
fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use aether_chassis::bundle_pack::ChassisSettings;
    use aether_chassis::package::{Sha256, decode_manifest};
    use sha2::{Digest, Sha256 as Sha256Hasher};

    use std::path::{Path, PathBuf};

    use super::inventory::{discover_behaviors, discover_components};
    use super::{
        BundleChassis, ComponentSource, PackComponent, Profile, build_planned_components, emit_depot,
        resolve_bundle_plan, write_pack,
    };

    #[test]
    fn emitted_depot_round_trips_through_decoder() {
        // Tripwire: the depot xtask writes must be readable by the chassis's
        // own `decode_manifest`, and every manifest reference must resolve
        // against `pack/objects` and re-hash to its filename. This catches
        // the emit bugs the target owns — a wrong object filename, a dropped
        // entry, a hash/bytes mismatch, or an encode that its own decoder
        // can't read — using the merged decoder as the oracle. It does not
        // re-test `encode_manifest`/`decode_manifest` symmetry (owned and
        // tested in aether-chassis); it tests that xtask's on-disk layout is
        // what that decoder consumes.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let out = env::temp_dir().join(format!("aether-xtask-package-{}-{seq}", process::id()));

        let chassis_src = env::temp_dir().join(format!("aether-xtask-chassis-{}-{seq}", process::id()));
        fs::write(&chassis_src, b"chassis-binary-bytes").expect("write fake chassis binary");

        // Two distinct components plus a third sharing bytes with the first —
        // the shared-bytes case exercises content-address dedup (one object
        // file, two entries pointing at it).
        let named = |name: &str, wasm: Vec<u8>| PackComponent {
            wasm,
            config: None,
            name: Some(name.to_owned()),
            export: None,
            replicas: None,
        };
        let components = vec![
            named("alpha", vec![0x00, 0x61, 0x73, 0x6d, 1, 2, 3]),
            named("beta", vec![9, 9, 9, 9]),
            named("alpha_twin", vec![0x00, 0x61, 0x73, 0x6d, 1, 2, 3]),
        ];
        let manifest =
            emit_depot(&out, &chassis_src, "aether-substrate", &components, ChassisSettings::default()).expect("emit");

        assert!(out.join("aether-substrate").exists(), "chassis binary copied into the depot root");

        let manifest_bytes = fs::read(out.join("pack").join("manifest")).expect("read pack/manifest");
        let decoded = decode_manifest(&manifest_bytes).expect("chassis decoder reads the emitted manifest");
        assert_eq!(decoded, manifest, "the decoded manifest equals what emit_depot wrote");

        let objects_dir = out.join("pack").join("objects");
        for entry in &decoded.entries {
            let object_path = objects_dir.join(entry.object.to_hex());
            let disk = fs::read(&object_path).unwrap_or_else(|_| panic!("object {} exists", entry.object.to_hex()));
            let mut hasher = Sha256Hasher::new();
            hasher.update(&disk);
            let recomputed = Sha256(hasher.finalize().into());
            assert_eq!(recomputed, entry.object, "object file content hashes to its filename");
        }

        // The shared-bytes entries resolve to one object; the two distinct
        // components plus the shared object make two object files.
        let object_count = fs::read_dir(&objects_dir).expect("read objects dir").count();
        assert_eq!(object_count, 2, "content-address dedup writes one file per distinct payload");

        fs::remove_dir_all(&out).ok();
        fs::remove_file(&chassis_src).ok();
    }

    #[test]
    fn write_pack_carries_config_object_settings_and_entry_order() {
        // The bundle path writes richer entries than `emit_depot` exercises —
        // a per-component config object plus the chassis settings (title /
        // window mode / tick rate) the standalone bins apply at boot. This
        // proves `write_pack` writes both the wasm and the config as distinct
        // content-addressed objects, threads the config hash onto the entry,
        // preserves entry order, and round-trips settings through the chassis's
        // own `decode_manifest` (the oracle). The bug it catches is a dropped
        // config object, a settings field lost on the way to the manifest, or a
        // reordered entry list.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("aether-xtask-writepack-{}-{seq}", process::id()));

        let components = vec![
            PackComponent {
                wasm: vec![0x00, 0x61, 0x73, 0x6d, 1],
                config: Some(vec![7, 8, 9]),
                name: Some("first".to_owned()),
                export: None,
                replicas: Some(2),
            },
            PackComponent {
                wasm: vec![0xfe, 0xff],
                config: None,
                name: None,
                export: Some("alt".to_owned()),
                replicas: None,
            },
        ];
        let settings = ChassisSettings {
            title: Some("bundle".to_owned()),
            window_mode: Some("windowed:800x600".to_owned()),
            tick_hz: Some(30),
        };
        let manifest = write_pack(&root, &components, settings.clone()).expect("write pack");

        let manifest_bytes = fs::read(root.join("pack").join("manifest")).expect("read pack/manifest");
        let decoded = decode_manifest(&manifest_bytes).expect("chassis decoder reads the pack manifest");
        assert_eq!(decoded, manifest, "the decoded manifest equals what write_pack wrote");
        assert_eq!(decoded.settings, settings, "chassis settings round-trip");
        assert_eq!(decoded.entries.len(), 2);
        assert!(decoded.entries[0].config.is_some(), "the first entry carries a config object");
        assert_eq!(decoded.entries[0].name.as_deref(), Some("first"));
        assert_eq!(decoded.entries[0].replicas, Some(2));
        assert_eq!(decoded.entries[1].config, None, "the config-less entry has no config hash");
        assert_eq!(decoded.entries[1].export.as_deref(), Some("alt"));

        // The first entry's wasm + config are two distinct objects; the second
        // entry's wasm is a third — three object files, none shared.
        let objects_dir = root.join("pack").join("objects");
        let object_count = fs::read_dir(&objects_dir).expect("read objects dir").count();
        assert_eq!(object_count, 3, "distinct wasm and config payloads each write one object");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn spec_driven_emit_carries_selected_entries_and_settings() {
        // A `--spec` product emit must carry each selected component's name /
        // export / config and the chassis settings all the way through the
        // chassis's own `decode_manifest`, resolve the spec's relative paths
        // against the spec file's directory, and ship the chosen (here
        // headless) chassis binary. Prebuilt-wasm entries keep the test off
        // cargo. The bugs it catches: a spec field dropped before the
        // manifest, a relative path anchored to the process cwd instead of the
        // spec dir (the prebuilt read would miss the file), the wrong chassis
        // bin shipped, or a config not written as its own object.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-xtask-spec-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create spec dir");

        fs::write(dir.join("alpha.wasm"), [0x00, 0x61, 0x73, 0x6d, 1]).expect("write alpha wasm");
        fs::write(dir.join("beta.wasm"), [0x00, 0x61, 0x73, 0x6d, 2]).expect("write beta wasm");
        fs::write(dir.join("alpha.cfg"), [7, 7, 7]).expect("write alpha config");

        // Component paths are relative — they must resolve against the spec
        // file's directory, not the process cwd.
        let spec = r#"{
            "chassis": "headless",
            "title": "loco-motion",
            "tick_hz": 30,
            "components": [
                { "wasm": "alpha.wasm", "config": "alpha.cfg", "name": "first", "export": "entry" },
                { "wasm": "beta.wasm" }
            ]
        }"#;
        let spec_path = dir.join("depot.json");
        fs::write(&spec_path, spec).expect("write spec");

        // `--chassis desktop` is the flag default; the spec's `headless` wins.
        let plan = resolve_bundle_plan(Some(&spec_path), BundleChassis::Desktop, &[], &[], None, None, None)
            .expect("resolve spec plan");
        assert_eq!(plan.chassis, BundleChassis::Headless, "spec chassis overrides the flag default");

        let (_, chassis_bin) = plan.chassis.package_chassis();
        assert_eq!(chassis_bin, "aether-substrate-headless", "headless selection ships the headless bin");

        let components = build_planned_components(&plan, Path::new("unused-for-prebuilt"), Profile::Release)
            .expect("read prebuilt components");

        let chassis_src = dir.join("fake-chassis");
        fs::write(&chassis_src, b"headless-binary-bytes").expect("write fake chassis");
        let out = dir.join("depot");
        let settings =
            ChassisSettings { title: plan.title.clone(), window_mode: plan.window_mode.clone(), tick_hz: plan.tick_hz };
        let manifest = emit_depot(&out, &chassis_src, chassis_bin, &components, settings).expect("emit depot");

        let manifest_bytes = fs::read(out.join("pack").join("manifest")).expect("read manifest");
        let decoded = decode_manifest(&manifest_bytes).expect("chassis decoder reads the emitted manifest");
        assert_eq!(decoded, manifest, "the decoded manifest equals what emit_depot wrote");
        assert_eq!(decoded.settings.title.as_deref(), Some("loco-motion"), "spec title rides into the manifest");
        assert_eq!(decoded.settings.tick_hz, Some(30), "spec tick rate rides into the manifest");
        assert_eq!(decoded.entries.len(), 2);
        assert_eq!(decoded.entries[0].name.as_deref(), Some("first"));
        assert_eq!(decoded.entries[0].export.as_deref(), Some("entry"));
        assert!(decoded.entries[0].config.is_some(), "the first entry's config rode into the manifest");
        assert_eq!(decoded.entries[1].name, None, "the config-less entry carries no name");
        assert_eq!(decoded.entries[1].config, None, "the config-less entry has no config object");

        assert!(out.join("aether-substrate-headless").exists(), "the headless chassis bin is shipped into the depot");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flag_plan_pairs_configs_by_position_and_classifies_sources() {
        // The flag path pairs `--config` values to `--components` by position,
        // leaves a trailing component config-less, classifies a `.wasm`
        // argument as a prebuilt path and anything else as a package name, and
        // rejects more configs than components. Pure input normalization the
        // crate owns.
        let components = vec!["aether-kit-commons".to_owned(), "build/probe.wasm".to_owned()];
        let configs = vec![PathBuf::from("camera.cfg")];
        let plan =
            resolve_bundle_plan(None, BundleChassis::Desktop, &components, &configs, Some("loco"), None, Some(60))
                .expect("resolve flag plan");

        assert_eq!(plan.title.as_deref(), Some("loco"));
        assert_eq!(plan.tick_hz, Some(60));
        assert_eq!(plan.components.len(), 2);
        assert!(matches!(&plan.components[0].source, ComponentSource::Package(p) if p == "aether-kit-commons"));
        assert!(matches!(&plan.components[1].source, ComponentSource::Prebuilt(_)), "a .wasm arg is a prebuilt path");
        assert_eq!(
            plan.components[0].config.as_deref(),
            Some(Path::new("camera.cfg")),
            "the config pairs to the first component by position",
        );
        assert_eq!(plan.components[1].config, None, "the trailing component is config-less");

        let excess = vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        let err = resolve_bundle_plan(None, BundleChassis::Desktop, &components, &excess, None, None, None)
            .expect_err("more configs than components is rejected");
        assert!(err.to_string().contains("pair by position"), "excess configs are rejected: {err}");
    }

    #[test]
    fn spec_requires_exactly_one_of_package_or_wasm() {
        // Each spec component must carry `package` XOR `wasm`; both-or-neither
        // is an authoring error the resolver rejects before any build runs.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-xtask-specxor-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create dir");

        let both = dir.join("both.json");
        fs::write(&both, r#"{ "components": [ { "package": "p", "wasm": "p.wasm" } ] }"#).expect("write both spec");
        let err = resolve_bundle_plan(Some(&both), BundleChassis::Desktop, &[], &[], None, None, None)
            .expect_err("package and wasm together is rejected");
        assert!(err.to_string().contains("exactly one of"), "package+wasm rejected: {err}");

        let neither = dir.join("neither.json");
        fs::write(&neither, r#"{ "components": [ { "name": "n" } ] }"#).expect("write neither spec");
        let err = resolve_bundle_plan(Some(&neither), BundleChassis::Desktop, &[], &[], None, None, None)
            .expect_err("neither package nor wasm is rejected");
        assert!(err.to_string().contains("exactly one of"), "neither package nor wasm rejected: {err}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovers_expected_component_set() {
        let metadata = cargo_metadata::MetadataCommand::new().no_deps().exec().expect("run cargo metadata");
        let components = discover_components(&metadata);
        let stems: BTreeSet<&str> = components.iter().map(|c| c.stem.as_str()).collect();

        // Parity with the structural sweep CI runs before this xtask: a drop
        // here surfaces as an AETHER_REQUIRE_RUNTIME panic. The
        // test fixtures are three single-output cdylib crates discovered the
        // same way as `aether-kit-commons` — no example path.
        for expected in [
            "aether_test_fixtures_bundle",
            "aether_test_fixtures_stateful_typed",
            "aether_test_fixtures_stateful_reshaped",
            "aether_kit_commons",
        ] {
            assert!(stems.contains(expected), "discovery dropped component {expected}; found {stems:?}");
        }

        // The fixture crates are lib cdylibs, not `[[example]]` targets, so
        // they need no example-build special-casing.
        for fixture in [
            "aether_test_fixtures_bundle",
            "aether_test_fixtures_stateful_typed",
            "aether_test_fixtures_stateful_reshaped",
        ] {
            let component =
                components.iter().find(|c| c.stem == fixture).unwrap_or_else(|| panic!("fixture {fixture} discovered"));
            assert!(!component.from_example, "fixture {fixture} is a lib cdylib, not an example target");
        }

        // aether-actor's own example cdylibs are NOT components — the
        // crate does not depend on itself, so it fails the actor-dep gate.
        for excluded in ["hello", "input_logger"] {
            assert!(!stems.contains(excluded), "discovery wrongly included aether-actor example {excluded}");
        }
    }

    #[test]
    fn discovers_behavior_fixtures_and_excludes_components() {
        let metadata = cargo_metadata::MetadataCommand::new().no_deps().exec().expect("run cargo metadata");
        let behaviors = discover_behaviors(&metadata);
        let stems: BTreeSet<&str> = behaviors.iter().map(|b| b.stem.as_str()).collect();

        // The #2688 fixture crate's example cdylibs depend on `aether-behavior`
        // and never `aether-actor`, so the behavior pass discovers each.
        for expected in ["intercept_slider", "intercept_slider_v2", "trap_script"] {
            assert!(stems.contains(expected), "behavior discovery dropped {expected}; found {stems:?}");
        }

        // The disjointness guard: `aether-kit-widget` declares an optional
        // `aether-behavior` dep (its `behavior` feature) AND an unconditional
        // `aether-actor` dep, and `cargo metadata` lists optional deps — so a
        // rule keyed on `aether-behavior` alone would sweep the widget crate in.
        // The `aether-actor`-absence guard keeps it a component, not a behavior.
        assert!(
            !stems.contains("aether_kit_widget"),
            "the actor-absence guard must exclude aether-kit-widget (a component) from behaviors; \
             found {stems:?}",
        );

        // Every discovered behavior is an `[[example]]` cdylib, so no lib-cdylib
        // special-casing on the build side.
        for behavior in &behaviors {
            assert!(behavior.from_example, "behavior {} is an [[example]] cdylib", behavior.stem);
        }
    }
}
