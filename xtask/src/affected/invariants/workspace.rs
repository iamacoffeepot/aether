//! The workspace as these checks see it: package roots, the `tests/`
//! directory each one owns, and the same wasm-source / dist-consumer
//! classification `cargo xtask affected` computes at run time.
//!
//! The classification is built from the production functions rather than
//! restated, so a check that says "this package is not a dist consumer"
//! is reporting what the tool will actually decide on the next PR.

use std::collections::BTreeSet;
use std::path::Path;

use cargo_metadata::camino::Utf8Path;
use cargo_metadata::{Metadata, MetadataCommand, Package, TargetKind};

use crate::affected::rules::global_screen;
use crate::affected::select::is_dist_consumer;
use crate::inventory::{discover_behaviors, discover_components};

/// The directory, relative to a package root, holding its
/// integration-test targets.
pub(super) const TEST_DIR: &str = "tests";

pub(super) struct Workspace {
    metadata: Metadata,
    /// Packages whose wasm `cargo xtask dist` builds.
    pub(super) wasm_sources: BTreeSet<String>,
}

impl Workspace {
    pub(super) fn load() -> Self {
        let metadata = MetadataCommand::new().no_deps().exec().expect("run cargo metadata");
        let wasm_sources = discover_components(&metadata)
            .into_iter()
            .map(|component| component.package)
            .chain(discover_behaviors(&metadata).into_iter().map(|behavior| behavior.package))
            .collect();
        Self { metadata, wasm_sources }
    }

    pub(super) fn root(&self) -> &Path {
        self.metadata.workspace_root.as_std_path()
    }

    pub(super) fn packages(&self) -> impl Iterator<Item = &Package> {
        self.metadata.packages.iter()
    }

    /// `path` rendered relative to the workspace root, for messages.
    pub(super) fn relative(&self, path: &Path) -> String {
        path.strip_prefix(self.root()).unwrap_or(path).display().to_string()
    }

    /// The package whose own `tests/` directory contains `path`.
    pub(super) fn tests_dir_owner(&self, path: &Path) -> Option<&Package> {
        self.packages().find(|package| path.starts_with(package_root(package).join(TEST_DIR)))
    }

    /// Whether any change inside this package forces the full suite
    /// before selection runs at all — [`global_screen`] hits on its
    /// directory. Such a package cannot be under-selected, so the
    /// dist-consumer checks have nothing to protect it from. `xtask`
    /// itself is the case: the selection machinery is screened to
    /// `run_all` precisely so a change to it can never narrow anything.
    ///
    /// Derived from the screen rather than named, so the exemption
    /// evaporates the moment the screen stops covering the package.
    pub(super) fn changes_force_run_all(&self, package: &Package) -> bool {
        let manifest = self.relative(package.manifest_path.as_std_path());
        global_screen(&[manifest]).is_some()
    }

    /// Whether `cargo xtask affected` classifies this package as needing
    /// the `cargo xtask dist` pre-build — the production predicate, over
    /// the package's own declared dependencies.
    pub(super) fn is_classified_dist_consumer(&self, package: &Package) -> bool {
        self.wasm_sources.contains(package.name.as_str())
            || is_dist_consumer(
                package.dependencies.iter().map(|dependency| dependency.name.as_str()),
                &self.wasm_sources,
            )
    }
}

pub(super) fn package_root(package: &Package) -> &Path {
    package.manifest_path.parent().map_or_else(|| Path::new(""), Utf8Path::as_std_path)
}

/// Whether the package exposes a library another package can depend on.
/// A bin-only package is nobody's dependency, so nothing it does outside
/// its own test code can propagate to another package's test run.
pub(super) fn has_library(package: &Package) -> bool {
    package.targets.iter().any(|target| {
        target.kind.iter().any(|kind| {
            matches!(
                kind,
                TargetKind::Lib
                    | TargetKind::RLib
                    | TargetKind::DyLib
                    | TargetKind::CDyLib
                    | TargetKind::StaticLib
                    | TargetKind::ProcMacro
            )
        })
    })
}
