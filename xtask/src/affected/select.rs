//! The graph analysis: map changed paths onto the workspace package graph,
//! take the reverse-dependency closure, and inject the wasm runtime coupling
//! cargo's own graph cannot see.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use determinator::Determinator;
use determinator::rules::DeterminatorRules;
use guppy::graph::{DependencyDirection, PackageGraph};

use crate::affected::rules::PATH_RULES_TOML;
use crate::affected::test_targets;

/// The harness crates that resolve a `dist`-produced artifact by
/// filesystem path rather than through a cargo dependency edge, so a
/// dev-dep on one is the structural signal that a package's tests need the
/// `cargo xtask dist` pre-build:
///
/// - `aether-harness-fleet` forks the dist-resolved headless chassis
///   binary (issue #3767) — the aether-fleet tests are the canonical case,
///   and they load no wasm at all.
/// - `aether-harness-substrate` owns `test_helpers::locate_component_wasm`,
///   which probes `target/wasm32-unknown-unknown/` for a component the
///   pre-build put there; `aether-harness-substrate-capture` re-exports it
///   and is how every current caller reaches it.
///
/// Over-inclusion here costs a wasm build and never correctness, so the
/// rule keys on the harness dep rather than on which of its helpers a given
/// test happens to call.
///
/// The list is not trusted: `cargo test -p xtask` derives it from the
/// workspace sources and fails when the two disagree, in either direction
/// (issue #4215, `crate::affected::invariants::dist_consumers`).
pub(super) const DIST_RESOLVING_HARNESSES: &[&str] =
    &["aether-harness-fleet", "aether-harness-substrate", "aether-harness-substrate-capture"];

/// The computed test selection.
pub struct Selection {
    /// `Some(reason)` when the whole workspace suite must run.
    pub run_all: Option<String>,
    /// Affected workspace package names (empty when `run_all` is set —
    /// the full invocation ignores the list).
    pub packages: BTreeSet<String>,
    /// Whether the `cargo xtask dist` wasm pre-build is needed before
    /// the tests run — see [`derive_wasm_needed`].
    pub wasm_needed: bool,
}

/// Map changed paths onto the package graph and take the
/// reverse-dependency closure, then inject the wasm runtime coupling.
///
/// The same graph is passed as determinator's old and new state: its
/// dual-graph analysis exists to catch manifest reshapes, and every path
/// that could reshape the graph is already screened to `run_all` by
/// [`crate::affected::rules::global_screen`] (a member-crate dependency edit
/// always touches `Cargo.lock`).
///
/// Paths confined to a package's own integration-test targets bypass the
/// determinator and select their owner directly — a test binary has no
/// dependents, so its closure is empty (issue #4197, see
/// [`crate::affected::test_targets`]).
pub(super) fn select(
    graph: &PackageGraph,
    changed: &[String],
    wasm_sources: &BTreeSet<String>,
    wasm_consumers: &BTreeSet<String>,
) -> Result<Selection> {
    let split = test_targets::partition(graph, changed);

    let rules = DeterminatorRules::parse(PATH_RULES_TOML).context("parse built-in determinator path rules")?;
    let mut determinator = Determinator::new(graph, graph);
    determinator.set_rules(&rules).context("apply determinator path rules")?;
    determinator.add_changed_paths(split.graph_paths.iter().copied());

    let affected = determinator.compute().affected_set;
    let mut packages: BTreeSet<String> = affected
        .packages(DependencyDirection::Forward)
        .filter(guppy::graph::PackageMetadata::in_workspace)
        .map(|package| package.name().to_string())
        .collect();
    packages.extend(split.test_target_packages);

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
/// execute that crate's wasm; a dep on one of
/// [`DIST_RESOLVING_HARNESSES`] means the tests reach a dist artifact by
/// path instead (issue #3766).
///
/// The harness half of that predicate is load-bearing once the affected set
/// narrows (#4197). `aether-chassis-headless`'s autoload test locates
/// `probe.wasm` through the capture harness and hard-fails under
/// `AETHER_REQUIRE_RUNTIME` without the pre-build, yet the package deps no
/// wasm source — under the old closure it got its `wasm_needed` by
/// accident, from an unrelated dependent that happened to be selected
/// alongside it.
pub(super) fn is_dist_consumer<'a>(
    mut dependency_names: impl Iterator<Item = &'a str>,
    wasm_sources: &BTreeSet<String>,
) -> bool {
    dependency_names.any(|name| wasm_sources.contains(name) || DIST_RESOLVING_HARNESSES.contains(&name))
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
    fn harness_dependents_are_dist_consumers() {
        // Issue #3766: a fleet-test host (aether-fleet is the canonical
        // case) loads no wasm, but its tests fork the dist-resolved
        // headless chassis binary through aether-harness-fleet — the
        // consumer predicate must catch the harness dep on its own,
        // else the tests hard-fail in CI with no `dist/bin` to fork.
        // Issue #4197 extends that to the substrate harnesses, whose
        // `locate_component_wasm` probes the pre-build's wasm output by
        // path: aether-chassis-headless reaches it that way and deps no
        // wasm source, so a dep-on-a-source rule alone leaves its autoload
        // test with nothing to load.
        let wasm_sources = string_set(&["aether-test-fixtures-bundle"]);
        for harness in ["aether-harness-fleet", "aether-harness-substrate", "aether-harness-substrate-capture"] {
            assert!(
                is_dist_consumer([harness].into_iter(), &wasm_sources),
                "a {harness} dependent needs the dist pre-build without any wasm dep"
            );
        }
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

        // Issue #4197: an integration test compiles into its own binary
        // that nothing links against, so a change confined to one selects
        // its own package and no dependent — while the same package's
        // library source keeps the full closure. A regression either way
        // (a closure that survives the narrowing, or a src change that
        // loses it) shows up as these two sets converging.
        let test_only = select(
            &graph,
            &strings(&["crates/aether-component/tests/inline_child.rs"]),
            &no_wasm_sources,
            &no_wasm_consumers,
        )
        .expect("select over test-only change");
        assert_eq!(
            test_only.packages,
            string_set(&["aether-component"]),
            "a test-only change selects its own package alone"
        );

        let library =
            select(&graph, &strings(&["crates/aether-component/src/lib.rs"]), &no_wasm_sources, &no_wasm_consumers)
                .expect("select over library change");
        assert!(
            library.packages.len() > test_only.packages.len(),
            "a library change in the same package keeps its reverse-dependency closure"
        );

        // The approval-policy.yml rule maps the cross-boundary test input to
        // its reader instead of falling back to run-everything.
        let policy = select(&graph, &strings(&["approval-policy.yml"]), &no_wasm_sources, &no_wasm_consumers)
            .expect("select over approval policy change");
        assert!(policy.run_all.is_none(), "the approval policy maps to a package, not run_all");
        assert!(
            policy.packages.contains("aether-chassis-bloomery"),
            "an approval policy change must select its reader"
        );
    }
}
