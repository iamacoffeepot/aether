//! The "a package's own integration tests have no dependents" narrowing
//! (issue #4197).
//!
//! A file under `<package-root>/tests/` compiles into that package's own
//! integration-test targets and nothing else: nothing links against a test
//! binary, so the file cannot change the package's public surface and its
//! reverse-dependency closure is empty. Feeding such a path to the
//! determinator marks the package changed and pulls in every dependent —
//! for `crates/aether-component/tests/`, 31 of the workspace's 58 packages,
//! whose test suites the edit provably cannot reach. This module splits
//! those paths back out so they select their own package directly instead.
//!
//! The match is deliberately narrow. Only the `tests/` directory
//! immediately under a workspace package's root qualifies, resolved from
//! guppy's own workspace paths rather than a `crates/*/tests/` glob. A
//! `src/**/tests/` module directory (`crates/aether-mcp/src/tools/tests/`)
//! is a unit-test module compiled as part of the library and keeps its full
//! closure.

use std::collections::{BTreeMap, BTreeSet};

use guppy::graph::PackageGraph;

/// The directory, relative to a package root, that cargo autodiscovers
/// integration-test targets from.
const TEST_DIR: &str = "tests";

/// Changed paths split by whether they are confined to a workspace
/// package's own integration-test targets.
pub(super) struct Partition<'a> {
    /// Packages owning a changed `tests/` path. These select directly — no
    /// reverse-dependency closure.
    pub(super) test_target_packages: BTreeSet<String>,
    /// Every other changed path, for the determinator to resolve normally.
    pub(super) graph_paths: Vec<&'a str>,
}

/// Split `changed` into the paths under some workspace package's own
/// `tests/` directory and everything else.
///
/// A package whose root path is empty (a package at the workspace root) is
/// skipped: the workspace has no such member, and admitting one would make
/// a top-level `tests/` tree claim it.
pub(super) fn partition<'a>(graph: &PackageGraph, changed: &'a [String]) -> Partition<'a> {
    let roots: BTreeMap<String, &str> = graph
        .workspace()
        .iter()
        .filter_map(|package| {
            let root = package.source().workspace_path()?.as_str();
            (!root.is_empty()).then(|| (format!("{root}/{TEST_DIR}/"), package.name()))
        })
        .collect();

    let mut test_target_packages = BTreeSet::new();
    let mut graph_paths = Vec::new();
    for path in changed.iter().map(String::as_str) {
        match roots.iter().find(|(prefix, _)| path.starts_with(prefix.as_str())) {
            Some((_, owner)) => {
                test_target_packages.insert((*owner).to_string());
            }
            None => graph_paths.push(path),
        }
    }
    Partition { test_target_packages, graph_paths }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use guppy::graph::PackageGraph;

    use super::partition;

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn graph() -> PackageGraph {
        guppy::MetadataCommand::new().build_graph().expect("build package graph")
    }

    #[test]
    fn only_the_package_root_tests_directory_splits_out() {
        // The narrowing is only sound for a package's own integration-test
        // targets. A `src/**/tests/` module compiles into the library, so
        // a prefix match loose enough to claim it would deselect real
        // dependents — the one failure mode this tool must never have.
        let changed = strings(&[
            "crates/aether-component/tests/inline_child.rs",
            "crates/aether-actor-derive/tests/ui/rejects_bare_handler_wasm.stderr",
            "crates/aether-mcp/src/tools/tests/terrain.rs",
            "crates/aether-component/src/lib.rs",
            "docs/guide/testing.md",
        ]);
        let split = partition(&graph(), &changed);

        assert_eq!(
            split.test_target_packages,
            strings(&["aether-actor-derive", "aether-component"]).into_iter().collect::<BTreeSet<String>>(),
            "package-root tests/ paths select their owner directly"
        );
        assert_eq!(
            split.graph_paths,
            vec![
                "crates/aether-mcp/src/tools/tests/terrain.rs",
                "crates/aether-component/src/lib.rs",
                "docs/guide/testing.md"
            ],
            "a src/**/tests/ module and every non-test path stay with the determinator"
        );
    }
}
