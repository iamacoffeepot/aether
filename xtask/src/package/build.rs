use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs;
use std::path::{Path, PathBuf};

use aether_chassis::autoload::{actor_lineage, selectable_exports};
use aether_chassis::encode_config_json;
use aether_chassis::package::{NamespacePath, nested_object_paths};
use aether_data::ActorLineageRecord;
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
    /// A file of init-config bytes, taken verbatim.
    pub(super) config: Option<PathBuf>,
    /// A JSON init-config file, encoded against the component's own
    /// declared `Config` schema. At most one of the two is set.
    pub(super) config_json: Option<PathBuf>,
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

/// One authored entry of a plan's named objects: what ships for a running
/// engine to read by path, without being loaded at boot.
#[derive(Debug)]
pub(super) enum PlannedObject {
    /// One object from `source`, shipped at `path`.
    Single { source: ComponentSource, path: NamespacePath },
    /// Every file beneath the directory `from`, each shipped at
    /// `<under>/<its relative path>`.
    Dir { from: PathBuf, under: NamespacePath },
}

/// The discover-everything dev sweep component set: build every
/// structurally discovered component and read its wasm into one unnamed
/// [`PackComponent`] per selectable export that declares `root`, each naming
/// that export's namespace (every spawn names its namespace, ADR-0241 §9) and
/// loaded under its type's own name (a load names no key for a singleton,
/// ADR-0241 §5). An export the lineage does not root would be refused at the
/// root, so the sweep prints each one it skips by namespace. A module whose
/// exports or lineage cannot be read fails the sweep naming the component.
/// Stem-sorted so a rebuild of the same sources yields a byte-identical
/// `pack/manifest`; each package builds in its own cargo invocation (never
/// batch multiple `-p`, see `inventory::build_plans`).
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
    let mut swept = Vec::new();
    for component in &components {
        let src = wasm_artifact_path(&wasm_profile_dir, component);
        let wasm = fs::read(&src).with_context(|| format!("read component wasm {}", src.display()))?;
        let exports = selectable_exports(&wasm)
            .map_err(|error| anyhow::anyhow!("read the exports of component {}: {error}", component.stem))?;
        let lineage = actor_lineage(&wasm)
            .map_err(|error| anyhow::anyhow!("read the placement lineage of component {}: {error}", component.stem))?;
        let (rooted, skipped): (Vec<_>, Vec<_>) =
            exports.into_iter().partition(|export| declares_root(&lineage, export));

        for export in &skipped {
            println!("package: sweep skips {export} ({}): it does not declare `root`", component.stem);
        }

        swept.extend(rooted.into_iter().map(|export| PackComponent {
            wasm: wasm.clone(),
            config: None,
            name: None,
            export: Some(export),
            replicas: None,
        }));
    }
    Ok(swept)
}

/// Whether the lineage records `export` as placeable at the root (ADR-0241 §5).
fn declares_root(lineage: &[ActorLineageRecord], export: &str) -> bool {
    lineage.iter().any(|record| matches!(record, ActorLineageRecord::Root { namespace, .. } if namespace == export))
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
        let wasm_path = locate_wasm(&component.source, target_dir, profile)?;
        let wasm = fs::read(&wasm_path).with_context(|| format!("read component wasm {}", wasm_path.display()))?;
        let config = match (&component.config, &component.config_json) {
            (Some(path), _) => {
                Some(fs::read(path).with_context(|| format!("read component config {}", path.display()))?)
            }
            (None, Some(path)) => {
                let json =
                    fs::read_to_string(path).with_context(|| format!("read component config {}", path.display()))?;
                let bytes = encode_config_json(&wasm, component.export.as_deref(), &json)
                    .with_context(|| format!("encode component config {}", path.display()))?;
                Some(bytes)
            }
            (None, None) => None,
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

/// The wasm file `source` names: a workspace package is built for wasm32
/// first, in its own cargo invocation, and a prebuilt artifact is located.
fn locate_wasm(source: &ComponentSource, target_dir: &Path, profile: Profile) -> Result<PathBuf> {
    match source {
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
            Ok(wasm)
        }
        ComponentSource::Prebuilt(path) => {
            fs::canonicalize(path).with_context(|| format!("locate prebuilt component wasm {}", path.display()))
        }
    }
}

/// Resolve the plan's named objects to each shipped path and the file whose
/// bytes ship there: a single object is built or located as a boot
/// component's wasm is, and a directory contributes every file beneath it.
/// Two entries resolving to one path fail the run naming both files, and so
/// does a path that is also the directory of another, which a directory of
/// built files could not mirror.
pub(super) fn resolve_named(
    plan: &PackagePlan,
    target_dir: &Path,
    profile: Profile,
) -> Result<BTreeMap<NamespacePath, PathBuf>> {
    let mut named = BTreeMap::new();
    for object in &plan.named {
        match object {
            PlannedObject::Single { source, path } => {
                insert_named(&mut named, path.clone(), locate_wasm(source, target_dir, profile)?)?;
            }
            PlannedObject::Dir { from, under } => insert_named_directory(&mut named, from, under)?,
        }
    }
    if let Some(nested) = nested_object_paths(&named) {
        let (object, beneath) = (nested.object, nested.beneath);
        bail!(
            "named object {object} ({}) is also a directory: {beneath} ({}) ships beneath it",
            named[object].display(),
            named[beneath].display(),
        );
    }
    Ok(named)
}

/// Add every file beneath `from` to `named` at `<under>/<its relative
/// path>`. A file whose resulting path is not a [`NamespacePath`] fails the
/// run naming the file; nothing is skipped.
///
/// Iterative, as the asset copy is: the tree's depth is the author's.
fn insert_named_directory(
    named: &mut BTreeMap<NamespacePath, PathBuf>,
    from: &Path,
    under: &NamespacePath,
) -> Result<()> {
    let root = fs::canonicalize(from).with_context(|| format!("locate named-object directory {}", from.display()))?;
    let mut pending = vec![root.clone()];

    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
            let file = entry.path();
            if entry.file_type().with_context(|| format!("stat {}", file.display()))?.is_dir() {
                pending.push(file);
                continue;
            }
            let relative = file.strip_prefix(&root).expect("every walked file is under the directory's root");
            insert_named(named, path_under(under, relative, &file)?, file)?;
        }
    }
    Ok(())
}

/// The path `file` ships at: `under`, then each component of its path
/// `relative` to the authored directory, joined by `/`.
fn path_under(under: &NamespacePath, relative: &Path, file: &Path) -> Result<NamespacePath> {
    let mut text = under.as_str().to_owned();
    for component in relative.components() {
        let segment = component
            .as_os_str()
            .to_str()
            .with_context(|| format!("named file {} has a name that is not UTF-8", file.display()))?;
        text.push('/');
        text.push_str(segment);
    }
    NamespacePath::new(&text).with_context(|| format!("named file {} cannot ship at {text:?}", file.display()))
}

/// Record that `file` ships at `path`, refusing a path already taken.
fn insert_named(named: &mut BTreeMap<NamespacePath, PathBuf>, path: NamespacePath, file: PathBuf) -> Result<()> {
    match named.entry(path) {
        Entry::Vacant(slot) => {
            slot.insert(file);
            Ok(())
        }
        Entry::Occupied(taken) => {
            bail!("named object {} is supplied by both {} and {}", taken.key(), taken.get().display(), file.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_root_needs_its_own_root_record() {
        let root = |namespace: &'static str| ActorLineageRecord::Root { actor: 1, namespace: namespace.into() };
        let child = ActorLineageRecord::Child {
            parent: 1,
            child: 2,
            parent_namespace: "aether.test.parent".into(),
            child_namespace: "aether.test.leaf".into(),
        };

        assert!(!declares_root(&[root("aether.test.other"), child.clone()], "aether.test.leaf"));
        assert!(declares_root(&[child, root("aether.test.leaf")], "aether.test.leaf"));
    }
}
