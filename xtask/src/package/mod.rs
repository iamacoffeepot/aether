//! `cargo xtask package` — emit the shippable depot layout (ADR-0163 §1):
//! the chassis binary, a persisted `pack/manifest`, and content-addressed
//! component objects under `pack/objects/<sha256>`. The Steam depot is this
//! directory uploaded verbatim.

mod build;
mod pack;
mod plan;

use std::path::PathBuf;

use aether_chassis::boot_manifest::ChassisSettings;
use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::Args;

use crate::cargo::{Profile, build_named_chassis, host_binary_filename};
use crate::inventory::PACKAGE_CHASSIS;
use crate::package::build::{build_planned_components, sweep_components};
use crate::package::pack::emit_depot;
use crate::package::plan::{PackageChassis, resolve_package_plan};

#[derive(Args)]
pub struct PackageArgs {
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
    #[arg(long, value_enum, default_value_t = PackageChassis::Desktop)]
    chassis: PackageChassis,
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

/// Emit the shippable depot layout (ADR-0163 §1): the chassis binary, a
/// persisted `pack/manifest`, and content-addressed component objects.
///
/// ```text
/// <out>/
///   aether-substrate            # chassis binary (desktop or headless; .exe on Windows)
///   pack/manifest               # `encode_manifest` output
///   pack/objects/<sha256>       # component wasm (+ config), content-addressed
/// ```
///
/// Two input surfaces resolve to the same emit (issue #4002):
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
pub fn run(args: &PackageArgs) -> Result<()> {
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;
    let target_dir = metadata.target_directory.as_std_path();
    let out = args.out.clone().unwrap_or_else(|| target_dir.join("package"));

    // A `--spec` file or an explicit `--components` set makes this a product
    // emit; with neither it is the discover-everything sweep.
    let selected = args.spec.is_some() || !args.components.is_empty();
    let (chassis_bin, components, settings) = if selected {
        let plan = resolve_package_plan(
            args.spec.as_deref(),
            args.chassis,
            &args.components,
            &args.configs,
            args.title.as_deref(),
            args.window_mode.as_deref(),
            args.tick_hz,
        )?;
        let (chassis_package, chassis_bin) = plan.chassis.substrate();
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

    // `package` builds host-target only (no `--target`), so cargo's on-disk
    // filename is the host platform's — `.exe` on Windows. The depot carries
    // that filename verbatim so the shipped binary is runnable as-is.
    let chassis_file = host_binary_filename(chassis_bin);
    let chassis_src = target_dir.join(args.profile.as_str()).join(&chassis_file);
    let manifest = emit_depot(&out, &chassis_src, &chassis_file, &components, settings)?;

    println!(
        "package: {} component object(s) + {} chassis bin -> {}",
        manifest.entries.len(),
        chassis_file,
        out.display(),
    );
    Ok(())
}
