use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cargo_metadata::Metadata;

use crate::cargo::{Profile, WASM_TARGET, build_command, build_component, run_status, wasm_artifact_path};
use crate::inventory::{build_plans, discover_components};
use crate::package::pack::PackComponent;
use crate::package::plan::PackagePlan;

/// One component in a resolved package plan: where its wasm comes from
/// plus the per-component load inputs that ride into the pack manifest.
#[derive(Debug)]
pub(super) struct PlannedComponent {
    pub(super) source: ComponentSource,
    pub(super) config: Option<PathBuf>,
    pub(super) name: Option<String>,
    pub(super) export: Option<String>,
}

/// Where a planned component's wasm comes from.
#[derive(Debug)]
pub(super) enum ComponentSource {
    /// A workspace package whose lib cdylib xtask builds for wasm32.
    Package(String),
    /// A prebuilt `.wasm` artifact supplied by path.
    Prebuilt(PathBuf),
}

/// The discover-everything dev sweep component set: build every
/// structurally discovered component and read its wasm into a name-labelled
/// [`PackComponent`]. Stem-sorted so a rebuild of the same sources yields a
/// byte-identical `pack/manifest`; each package builds in its own cargo
/// invocation (never batch multiple `-p`, see `inventory::build_plans`).
pub(super) fn sweep_components(metadata: &Metadata, target_dir: &Path, profile: Profile) -> Result<Vec<PackComponent>> {
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

/// Build (or locate) each planned component's wasm, in plan order, and read
/// its bytes plus any per-component config bytes into a [`PackComponent`].
/// One cargo invocation per package — never batch multiple `-p` (see
/// `inventory::build_plans` on the feature-unification trap).
pub(super) fn build_planned_components(
    plan: &PackagePlan,
    target_dir: &Path,
    profile: Profile,
) -> Result<Vec<PackComponent>> {
    let mut components = Vec::new();
    for component in &plan.components {
        let wasm_path = match &component.source {
            ComponentSource::Package(package) => {
                let mut wasm_cmd = build_command(profile);
                wasm_cmd.args(["--target", WASM_TARGET, "-p", package]);
                run_status(wasm_cmd, &format!("build component wasm for {package}"))?;
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
