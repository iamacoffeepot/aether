//! The workspace crate graph a crate-declared scope derives its surface from,
//! and the derivation itself.
//!
//! A declared surface is not a forecast of the files a lane will touch. Nobody
//! can compute that before the work is done — not the scoper, not the model,
//! not a static analysis — so a file list is a guess that the first honest
//! construct lap invalidates, and every such guess costs an amendment round
//! trip. What *is* computable is the blast radius of the crates the work is
//! about: those crates, plus every workspace crate that depends on them,
//! because a change inside a crate can only be observed by something that links
//! it.
//!
//! So `## Declared crates` names crates and this module turns them into globs.
//! The reverse-dependency closure is read from the workspace manifests rather
//! than from `cargo metadata`: the edges this needs are the workspace-local
//! `[dependencies]` keys, which are in the manifests verbatim, and reading them
//! directly keeps the scope parser free of a resolver, a lockfile, and a
//! subprocess.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// The roots every bloom may touch regardless of which crates it declared: the
/// build tooling, the operator scripts, the contributor guide, and the
/// workspace-level tests.
///
/// These are not derived from anything, because nothing links them — a crate
/// change that needs an `xtask` verb taught about it has no dependency edge
/// saying so. They are the standing allowance that keeps a bloom from having to
/// amend its surface to fix the tooling its own change broke.
pub(super) const SHARED_ROOTS: &[&str] = &["xtask/**", "scripts/**", "docs/guide/**", "tests/**"];

/// The workspace's crates and their workspace-local dependency edges.
pub(super) struct WorkspaceCrates {
    /// Crate name to its repository-relative directory (`crates/aether-data`).
    directories: BTreeMap<String, String>,
    /// Crate name to the workspace-local crates that depend on it.
    dependents: BTreeMap<String, BTreeSet<String>>,
}

impl WorkspaceCrates {
    /// Read the workspace rooted at `root` — its `Cargo.toml` `members` list,
    /// then each member manifest's package name and dependency keys.
    ///
    /// # Errors
    /// An unreadable or malformed workspace manifest, or a member manifest that
    /// names no package. A scope cannot be derived against a workspace that
    /// cannot be read, and guessing at the graph would silently narrow a
    /// surface.
    pub(super) fn load(root: &Path) -> Result<Self> {
        let manifest = read_manifest(&root.join("Cargo.toml"))?;
        let members = manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("members"))
            .and_then(toml::Value::as_array)
            .ok_or_else(|| anyhow!("{}: no [workspace] members list", root.join("Cargo.toml").display()))?;

        let mut directories = BTreeMap::new();
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for member in members.iter().filter_map(toml::Value::as_str) {
            let member_manifest = read_manifest(&root.join(member).join("Cargo.toml"))?;
            let package = member_manifest.get("package").and_then(|package| package.get("name"));
            let Some(name) = package.and_then(toml::Value::as_str) else {
                bail!("{member}/Cargo.toml names no package");
            };
            directories.insert(name.to_owned(), member.trim_end_matches('/').to_owned());
            edges.insert(name.to_owned(), dependency_names(&member_manifest));
        }

        // Invert once, keeping only edges whose target is a workspace member: a
        // registry dependency has no directory here, so a closure over it would
        // widen a surface onto a path the repository does not contain.
        let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, dependencies) in &edges {
            for dependency in dependencies.iter().filter(|dependency| directories.contains_key(*dependency)) {
                dependents.entry(dependency.clone()).or_default().insert(name.clone());
            }
        }

        Ok(Self { directories, dependents })
    }

    /// Find the workspace root by walking up from `start` to the first
    /// directory whose `Cargo.toml` declares a `[workspace]`.
    ///
    /// # Errors
    /// No ancestor carries a workspace manifest.
    pub(super) fn find_root(start: &Path) -> Result<PathBuf> {
        for ancestor in start.ancestors() {
            let manifest = ancestor.join("Cargo.toml");
            if manifest.is_file() && fs::read_to_string(&manifest).is_ok_and(|text| text.contains("[workspace]")) {
                return Ok(ancestor.to_path_buf());
            }
        }
        bail!("no workspace Cargo.toml at or above {}", start.display())
    }

    /// `declared` plus every workspace crate that transitively depends on one
    /// of them, in name order.
    ///
    /// Iterative rather than recursive: the depth is the workspace's dependency
    /// height, which is small today but is not bounded by anything except the
    /// manifests, and a queue costs nothing to write.
    ///
    /// # Errors
    /// The first declared name that is not a workspace crate. A typo'd crate
    /// name must refuse the scope rather than derive a surface missing the
    /// subtree the work is actually about.
    pub(super) fn closure(&self, declared: &[String]) -> Result<BTreeSet<String>> {
        let mut reached: BTreeSet<String> = BTreeSet::new();
        let mut pending: VecDeque<String> = VecDeque::new();
        for name in declared {
            if !self.directories.contains_key(name) {
                bail!("declared crate {name:?} is not a workspace crate");
            }
            if reached.insert(name.clone()) {
                pending.push_back(name.clone());
            }
        }

        while let Some(name) = pending.pop_front() {
            let Some(dependents) = self.dependents.get(&name) else {
                continue;
            };
            for dependent in dependents {
                if reached.insert(dependent.clone()) {
                    pending.push_back(dependent.clone());
                }
            }
        }

        Ok(reached)
    }

    /// The `dir/**` glob for one workspace crate, or `None` when it is not one.
    fn subtree(&self, name: &str) -> Option<String> {
        self.directories.get(name).map(|directory| format!("{directory}/**"))
    }
}

/// The declared surface a crate-declared scope resolves to: one subtree per
/// crate in the reverse-dependency closure, then the shared roots, then the
/// protected files.
///
/// Order is meaning, not cosmetics. The crate subtrees come first because they
/// are what the scope said; the shared roots follow as the standing allowance;
/// the protected literals come last because they are the entries the tier reads
/// and an operator scanning the rendered surface should find them together at
/// the end. Deduplicated, so a shared root that is also a declared crate's
/// subtree — `xtask` is a workspace member — appears once.
///
/// # Errors
/// A declared name that is not a workspace crate, or a protected path outside
/// the declared-surface grammar.
pub(super) fn derive_surface(
    workspace: &WorkspaceCrates,
    declared: &[String],
    protected: &[String],
) -> Result<Vec<String>> {
    let mut surface: Vec<String> = Vec::new();
    for name in workspace.closure(declared)? {
        push_unique(&mut surface, workspace.subtree(&name).unwrap_or_default());
    }
    for root in SHARED_ROOTS {
        push_unique(&mut surface, (*root).to_owned());
    }
    for path in protected {
        push_unique(&mut surface, path.clone());
    }
    Ok(surface)
}

fn push_unique(surface: &mut Vec<String>, glob: String) {
    if !glob.is_empty() && !surface.contains(&glob) {
        surface.push(glob);
    }
}

fn read_manifest(path: &Path) -> Result<toml::Table> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;

    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Every dependency key a manifest declares, across the three dependency
/// tables and any `[target.*]` block that carries them.
///
/// Keys, not resolved package names: a renamed dependency (`foo = { package =
/// "bar" }`) is read through its `package` field so the edge points at the
/// crate the workspace actually knows.
fn dependency_names(manifest: &toml::Table) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        collect_dependencies(manifest.get(table), &mut names);
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for platform in targets.values() {
            for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
                collect_dependencies(platform.get(table), &mut names);
            }
        }
    }
    names
}

fn collect_dependencies(table: Option<&toml::Value>, names: &mut BTreeSet<String>) {
    let Some(table) = table.and_then(toml::Value::as_table) else {
        return;
    };
    for (key, value) in table {
        let renamed = value.get("package").and_then(toml::Value::as_str);
        names.insert(renamed.unwrap_or(key).to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{WorkspaceCrates, derive_surface};

    /// A three-crate line: `leaf` <- `middle` <- `top`, plus an unrelated
    /// `island`.
    fn workspace() -> WorkspaceCrates {
        let directories = ["leaf", "middle", "top", "island"]
            .into_iter()
            .map(|name| (name.to_owned(), format!("crates/{name}")))
            .collect::<BTreeMap<_, _>>();
        let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        dependents.entry("leaf".to_owned()).or_default().insert("middle".to_owned());
        dependents.entry("middle".to_owned()).or_default().insert("top".to_owned());

        WorkspaceCrates { directories, dependents }
    }

    #[test]
    fn the_closure_walks_transitive_dependents_and_leaves_the_island() {
        let reached = workspace().closure(&["leaf".to_owned()]).expect("leaf is a workspace crate");

        assert!(reached.contains("leaf") && reached.contains("middle") && reached.contains("top"), "{reached:?}");
        assert!(
            !reached.contains("island"),
            "a crate that depends on nothing in the closure is outside the blast radius: {reached:?}"
        );
    }

    #[test]
    fn an_unknown_crate_refuses_rather_than_deriving_a_narrower_surface() {
        // A typo'd crate name that silently derived to the empty closure would
        // hand the lane a surface missing the subtree it was scoped to change,
        // and the first containment refusal would blame the lane.
        let error = workspace().closure(&["lief".to_owned()]).expect_err("a non-member must refuse");

        assert!(error.to_string().contains("lief"), "the refusal names the crate: {error}");
    }

    #[test]
    fn the_derived_surface_is_subtrees_then_roots_then_protected_files() {
        let surface = derive_surface(&workspace(), &["middle".to_owned()], &["Cargo.lock".to_owned()])
            .expect("middle is a workspace crate");

        assert_eq!(
            surface,
            ["crates/middle/**", "crates/top/**", "xtask/**", "scripts/**", "docs/guide/**", "tests/**", "Cargo.lock",],
            "the closure, then the standing roots, then the entries the tier reads"
        );
    }
}
