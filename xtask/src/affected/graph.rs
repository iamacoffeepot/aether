//! The workspace inputs the closure analysis runs over: the guppy package
//! graph plus the two wasm-coupling sets cargo's own edges do not express.
//!
//! Loaded once and handed to [`select`](crate::affected::select::select()), so
//! the two consumers — `cargo xtask affected` and the member verify lane
//! (#4890) — compute the same closure from the same inputs rather than each
//! assembling their own and drifting.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use guppy::graph::PackageGraph;

use crate::affected::select::{Selection, is_dist_consumer, select};
use crate::inventory::{discover_behaviors, discover_components};

/// The loaded workspace: the package graph, the crates whose sources compile
/// to wasm, and the crates whose tests need that wasm pre-built.
pub struct Workspace {
    graph: PackageGraph,
    wasm_sources: BTreeSet<String>,
    wasm_consumers: BTreeSet<String>,
}

impl Workspace {
    /// Build the package graph and derive both wasm sets from the same
    /// `cargo metadata` read.
    pub fn load() -> Result<Self> {
        let graph = guppy::MetadataCommand::new().build_graph().context("build guppy package graph")?;
        let metadata =
            cargo_metadata::MetadataCommand::new().no_deps().exec().context("run cargo metadata for inventory")?;
        let wasm_sources: BTreeSet<String> = discover_components(&metadata)
            .into_iter()
            .map(|component| component.package)
            .chain(discover_behaviors(&metadata).into_iter().map(|behavior| behavior.package))
            .collect();
        // A dist consumer needs the `cargo xtask dist` pre-build for either
        // artifact class it packages: component/behavior wasm (a dep on a
        // wasm source — the tests execute that crate's wasm), or the chassis
        // binaries (a dep on aether-harness-fleet, whose harness forks the
        // dist-resolved `aether-headless`; issue #3766).
        let wasm_consumers: BTreeSet<String> = metadata
            .packages
            .iter()
            .filter(|package| {
                is_dist_consumer(package.dependencies.iter().map(|dependency| dependency.name.as_str()), &wasm_sources)
            })
            .map(|package| package.name.to_string())
            .collect();

        Ok(Self { graph, wasm_sources, wasm_consumers })
    }

    /// Map `changed` onto the graph and take the reverse-dependency closure.
    pub fn select(&self, changed: &[String]) -> Result<Selection> {
        select(&self.graph, changed, &self.wasm_sources, &self.wasm_consumers)
    }

    /// Every workspace crate, by name.
    pub fn members(&self) -> BTreeSet<String> {
        self.graph.workspace().iter().map(|package| package.name().to_string()).collect()
    }

    /// Every workspace crate's root directory, each carrying its trailing
    /// separator so a prefix match names a path *inside* that crate rather
    /// than a sibling whose name merely starts the same way
    /// (`crates/aether-math-derive/…` against the `crates/aether-math` root).
    ///
    /// The verify lane reads this to bound the one claim a selection of no
    /// packages does not itself justify: the path rules deselect a handful of
    /// files that live inside a crate (`README*`, `LICENSE*`, `.gitignore`),
    /// and "no package was selected" reads the same for those as it does for a
    /// diff that never entered a crate at all. A package at the workspace root
    /// would have an empty path and is skipped — a root prefix of `/` would
    /// claim every path in the tree.
    pub fn crate_roots(&self) -> BTreeSet<String> {
        self.graph
            .workspace()
            .iter()
            .filter_map(|package| package.source().workspace_path().map(|root| root.as_str().to_owned()))
            .filter(|root| !root.is_empty())
            .map(|root| format!("{root}/"))
            .collect()
    }

    /// The crates whose sources compile to component or behavior wasm.
    ///
    /// The verify lane reads this to recognize the one coupling a linkage
    /// closure cannot: a test that loads a built `.wasm` through the
    /// filesystem links nothing at all against the crate that produced it.
    pub fn wasm_sources(&self) -> &BTreeSet<String> {
        &self.wasm_sources
    }

    /// Whether `package`'s tests need the `cargo xtask dist` pre-build.
    ///
    /// True when the crate compiles to component wasm, or when its tests
    /// consume a dist artifact (a dep on a wasm source or a dist-resolving
    /// harness). A crate with neither relationship must not force that
    /// cross-build.
    pub fn needs_dist_prepare(&self, package: &str) -> bool {
        self.wasm_sources.contains(package) || self.wasm_consumers.contains(package)
    }
}
