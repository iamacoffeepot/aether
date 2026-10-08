//! `cargo xtask package` — emit the shippable depot layout (ADR-0163 §1):
//! the chassis binary, the workspace license files, a persisted
//! `pack/manifest`, and content-addressed objects under
//! `pack/objects/<sha256>`: the components boot loads, and the named
//! objects a running engine reads by path. The Steam depot is this
//! directory uploaded verbatim.

mod build;
mod pack;
mod plan;

use std::collections::BTreeMap;
use std::path::PathBuf;

use aether_chassis::boot_manifest::ChassisSettings;
use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::Args;

use crate::cargo::{Profile, build_named_chassis, host_binary_filename};
use crate::inventory::PACKAGE_CHASSIS;
use crate::package::build::{build_planned_components, resolve_named, sweep_components};
use crate::package::pack::{PackContents, emit_depot};
use crate::package::plan::{FlagSelection, PackageChassis, resolve_package_plan};

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
    /// A directory of objects to ship without loading at boot: every file
    /// beneath `DIR` ships at `<UNDER>/<its relative path>`, and a running
    /// engine reads it there through the `objects` file namespace. Repeat
    /// the flag for more directories; a single object goes through
    /// `--spec`. Each resulting path is lowercase `a-z`, `0-9`, `.`, `_`,
    /// `-` segments joined by `/`, and a file that breaks that fails the run.
    #[arg(long, num_args = 2, value_names = ["UNDER", "DIR"], requires = "components")]
    named: Vec<String>,
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
    /// Render clear colour (desktop chassis only), sRGB `rrggbb` hex —
    /// the same vocabulary as `AETHER_RENDER_CLEAR_COLOR`. A line-drawing
    /// product ships its paper here.
    #[arg(long)]
    clear_color: Option<String>,
    /// Asset tree to ship, copied verbatim into `pack/assets`. The
    /// packaged chassis roots the `assets` namespace there, below an
    /// operator's `AETHER_ASSETS_DIR` / `--assets-dir` and above the
    /// compiled default, so a depot's components find the files their
    /// `aether.fs.read` paths name.
    #[arg(long)]
    assets: Option<PathBuf>,
    /// Full-fidelity depot spec (JSON) — alternative to the component
    /// and chassis-config flags. Carries chassis, `title` /
    /// `window_mode` / `tick_hz` / `clear_color`, per-component `package`-or-`wasm` +
    /// `config` + `name` + `export`, and a `named` list of objects to ship
    /// without loading (`package`, `wasm`, or `dir` entries); relative paths
    /// resolve against the spec file's directory.
    #[arg(
        long,
        conflicts_with_all = ["components", "configs", "named", "title", "window_mode", "tick_hz", "clear_color"]
    )]
    spec: Option<PathBuf>,
}

/// Emit the shippable depot layout (ADR-0163 §1): the chassis binary, the
/// workspace license files, a persisted `pack/manifest`, and content-addressed
/// objects.
///
/// ```text
/// <out>/
///   aether-desktop              # chassis binary (`aether-headless` under `--chassis headless`; .exe on Windows)
///   LICENSE-MIT                 # workspace licenses, shipped with the statically linked binary
///   LICENSE-APACHE
///   pack/manifest               # `encode_manifest` output
///   pack/objects/<sha256>       # component wasm (+ config) and named objects, content-addressed
///   pack/assets/…               # the `--assets` tree, verbatim
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
///   / `tick_hz` / `clear_color` riding into `pack/manifest`, plus any
///   named objects (`--named`, or a spec's `named` list).
///
/// Each object is referenced from the manifest by its sha256 hash, so
/// identity is the content and a name is a label. A named object is also
/// listed under the path a running engine reads it at, with its size; the
/// run prints one line per named object.
pub fn run(args: &PackageArgs) -> Result<()> {
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;
    let target_dir = metadata.target_directory.as_std_path();
    let out = args.out.clone().unwrap_or_else(|| target_dir.join("package"));

    // A `--spec` file or an explicit `--components` set makes this a product
    // emit; with neither it is the discover-everything sweep.
    let selected = args.spec.is_some() || !args.components.is_empty();
    let (chassis_bin, components, named, settings) = if selected {
        let flags = FlagSelection {
            chassis: args.chassis,
            components: &args.components,
            configs: &args.configs,
            named: &args.named,
            settings: ChassisSettings {
                title: args.title.clone(),
                window_mode: args.window_mode.clone(),
                tick_hz: args.tick_hz,
                clear_color: args.clear_color.clone(),
            },
        };
        let plan = resolve_package_plan(args.spec.as_deref(), flags)?;
        let (chassis_package, chassis_bin) = plan.chassis.substrate();
        let components = build_planned_components(&plan, target_dir, args.profile)?;
        let named = resolve_named(&plan, target_dir, args.profile)?;
        build_named_chassis(chassis_package, chassis_bin, args.profile)?;
        (chassis_bin, components, named, plan.settings)
    } else {
        let (chassis_package, chassis_bin) = PACKAGE_CHASSIS;
        let components = sweep_components(&metadata, target_dir, args.profile)?;
        build_named_chassis(chassis_package, chassis_bin, args.profile)?;
        (chassis_bin, components, BTreeMap::new(), ChassisSettings::default())
    };

    // `package` builds host-target only (no `--target`), so cargo's on-disk
    // filename is the host platform's — `.exe` on Windows. The depot carries
    // that filename verbatim so the shipped binary is runnable as-is.
    let chassis_file = host_binary_filename(chassis_bin);
    let chassis_src = target_dir.join(args.profile.as_str()).join(&chassis_file);
    let contents = PackContents { components: &components, named: &named, settings };
    let workspace_root = metadata.workspace_root.as_std_path();
    let manifest = emit_depot(&out, workspace_root, &chassis_src, &chassis_file, contents, args.assets.as_deref())?;

    for (path, object) in &manifest.named {
        println!("package: named path={path} sha256={} size={}", object.sha256, object.size);
    }
    println!(
        "package: {} component object(s) + {} named object(s) + {} chassis bin -> {}",
        manifest.entries.len(),
        manifest.named.len(),
        chassis_file,
        out.display(),
    );
    Ok(())
}
