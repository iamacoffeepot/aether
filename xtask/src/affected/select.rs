//! The graph analysis: map changed paths onto the workspace package graph,
//! take the reverse-dependency closure, and inject the wasm runtime coupling
//! cargo's own graph cannot see.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use determinator::Determinator;
use determinator::rules::DeterminatorRules;
use guppy::graph::{DependencyDirection, PackageGraph};

use crate::affected::rules::PATH_RULES_TOML;

/// The real-process fleet harness (issue #3767): any package that
/// dev-deps it forks the dist-resolved headless chassis binary at test
/// time, so its tests need the `cargo xtask dist` pre-build even when
/// they load no wasm at all (the aether-fleet fleet tests are the
/// canonical case).
const HARNESS_FLEET_PACKAGE: &str = "aether-harness-fleet";

/// The computed test selection.
pub(super) struct Selection {
    /// `Some(reason)` when the whole workspace suite must run.
    pub(super) run_all: Option<String>,
    /// Affected workspace package names (empty when `run_all` is set —
    /// the full invocation ignores the list).
    pub(super) packages: BTreeSet<String>,
    /// Whether the `cargo xtask dist` wasm pre-build is needed before
    /// the tests run — see [`derive_wasm_needed`].
    pub(super) wasm_needed: bool,
}

/// Map changed paths onto the package graph and take the
/// reverse-dependency closure, then inject the wasm runtime coupling.
///
/// The same graph is passed as determinator's old and new state: its
/// dual-graph analysis exists to catch manifest reshapes, and every path
/// that could reshape the graph is already screened to `run_all` by
/// [`crate::affected::rules::global_screen`] (a member-crate dependency edit
/// always touches `Cargo.lock`).
pub(super) fn select(
    graph: &PackageGraph,
    changed: &[String],
    wasm_sources: &BTreeSet<String>,
    wasm_consumers: &BTreeSet<String>,
) -> Result<Selection> {
    let rules = DeterminatorRules::parse(PATH_RULES_TOML).context("parse built-in determinator path rules")?;
    let mut determinator = Determinator::new(graph, graph);
    determinator.set_rules(&rules).context("apply determinator path rules")?;
    determinator.add_changed_paths(changed.iter().map(String::as_str));

    let affected = determinator.compute().affected_set;
    let packages: BTreeSet<String> = affected
        .packages(DependencyDirection::Forward)
        .filter(guppy::graph::PackageMetadata::in_workspace)
        .map(|package| package.name().to_string())
        .collect();
    if packages.len() == graph.workspace().iter().count() {
        return Ok(Selection {
            run_all: Some("every workspace package is affected".to_string()),
            packages: BTreeSet::new(),
            wasm_needed: true,
        });
    }

    let wasm_needed = derive_wasm_needed(&packages, wasm_sources, wasm_consumers);
    Ok(Selection { run_all: None, packages, wasm_needed })
}

/// Whether a package's dependency list makes its tests need the
/// `cargo xtask dist` pre-build: a dep on a wasm source means the tests
/// execute that crate's wasm; a dep on [`HARNESS_FLEET_PACKAGE`] means the
/// tests fork the dist-resolved headless chassis binary (issue #3766).
pub(super) fn is_dist_consumer<'a>(
    mut dependency_names: impl Iterator<Item = &'a str>,
    wasm_sources: &BTreeSet<String>,
) -> bool {
    dependency_names.any(|name| wasm_sources.contains(name) || name == HARNESS_FLEET_PACKAGE)
}

/// Whether the `cargo xtask dist` wasm pre-build must run before the
/// selected tests: the chassis package's scenario tests execute component
/// wasm, a wasm-source crate's own tests may read its wasm, and a crate
/// that depends on a wasm source can execute that source's wasm at test
/// time (issue #3617 — the original such consumer was `aether-chassis-bloomery`
/// running `aether-bloomery`'s control-core wasm; that retired when the control
/// core became a native cap, but the rule stays generic for any future consumer
/// that would hard-fail under `AETHER_REQUIRE_RUNTIME` without the pre-build).
fn derive_wasm_needed(
    packages: &BTreeSet<String>,
    wasm_sources: &BTreeSet<String>,
    wasm_consumers: &BTreeSet<String>,
) -> bool {
    packages.iter().any(|name| wasm_sources.contains(name) || wasm_consumers.contains(name))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{derive_wasm_needed, is_dist_consumer, select};

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn string_set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn wasm_needed_covers_consumers_beyond_the_chassis() {
        // The invariant from issue #3617: a crate that is neither the chassis
        // nor a wasm source, but whose tests execute a wasm source's component at
        // runtime, still needs the dist pre-build — deriving wasm_needed from
        // chassis membership alone would skip it and hard-fail under
        // AETHER_REQUIRE_RUNTIME. (The original instance was aether-chassis-bloomery
        // running aether-bloomery's control-core wasm; that retired when the
        // control core became a native cap, so the labels below are illustrative
        // — the generic rule stays for any future such consumer.)
        let wasm_sources = string_set(&["aether-bloomery"]);
        let wasm_consumers = string_set(&["aether-chassis-bloomery"]);
        assert!(
            derive_wasm_needed(&string_set(&["aether-chassis-bloomery"]), &wasm_sources, &wasm_consumers),
            "a selected wasm-consumer crate needs the pre-build"
        );
        assert!(
            derive_wasm_needed(&string_set(&["aether-bloomery"]), &wasm_sources, &wasm_consumers),
            "a selected wasm-source crate needs the pre-build"
        );
        assert!(
            !derive_wasm_needed(&string_set(&["aether-math"]), &wasm_sources, &wasm_consumers),
            "a crate with no wasm relationship must not force the pre-build"
        );
    }

    #[test]
    fn harness_fleet_dependents_are_dist_consumers() {
        // Issue #3766: a fleet-test host (aether-fleet is the canonical
        // case) loads no wasm, but its tests fork the dist-resolved
        // headless chassis binary through aether-harness-fleet — the
        // consumer predicate must catch the harness dep on its own,
        // else the tests hard-fail in CI with no `dist/bin` to fork.
        let wasm_sources = string_set(&["aether-test-fixtures-bundle"]);
        assert!(
            is_dist_consumer(["aether-harness-fleet"].into_iter(), &wasm_sources),
            "a harness-fleet dependent needs the dist pre-build without any wasm dep"
        );
        assert!(
            is_dist_consumer(["aether-test-fixtures-bundle"].into_iter(), &wasm_sources),
            "a wasm-source dependent stays a dist consumer"
        );
        assert!(
            !is_dist_consumer(["aether-math", "serde"].into_iter(), &wasm_sources),
            "unrelated deps must not force the pre-build"
        );
    }

    #[test]
    fn real_graph_closure_and_conservative_fallback() {
        let graph = guppy::MetadataCommand::new().build_graph().expect("build package graph");
        let no_wasm_sources = BTreeSet::new();
        let no_wasm_consumers = BTreeSet::new();

        // A leaf-crate change selects that crate but not the chassis
        // package — the payoff case this tool exists for. An inverted or
        // over-wide closure shows up here.
        let leaf = select(
            &graph,
            &strings(&["crates/aether-chassis-bloomery/src/lib.rs"]),
            &no_wasm_sources,
            &no_wasm_consumers,
        )
        .expect("select over leaf change");
        assert!(leaf.run_all.is_none(), "leaf change must not run everything");
        assert!(leaf.packages.contains("aether-chassis-bloomery"), "changed crate must be selected");

        // A path matching no package and no rule must fall back to the
        // whole workspace — silent deselection of unknown inputs is the
        // one failure mode this tool must never have.
        let unknown = select(&graph, &strings(&["mystery-toplevel-input.txt"]), &no_wasm_sources, &no_wasm_consumers)
            .expect("select over unknown path");
        assert!(unknown.run_all.is_some(), "unknown path must run everything");

        // The bloomery/** rule maps the cross-boundary test input to its
        // reader instead of falling back to run-everything.
        let policy = select(&graph, &strings(&["bloomery/approval-policy.yml"]), &no_wasm_sources, &no_wasm_consumers)
            .expect("select over bloomery config change");
        assert!(policy.run_all.is_none(), "bloomery config maps to a package, not run_all");
        assert!(policy.packages.contains("aether-chassis-bloomery"), "bloomery config change must select its reader");
    }
}
