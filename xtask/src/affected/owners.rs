//! Which workspace packages a changed set *wrote*, as opposed to which ones
//! it can have broken.
//!
//! The reverse-dependency closure answers the second question and is what the
//! verify lane narrows by. It cannot answer the first: a closure already holds
//! every dependent, so two members editing two unrelated crates in the same
//! subtree both carry the chassis in their closure and would read as touching
//! each other. Deriving a semantic edge between two members needs both halves —
//! one member's *written* packages against another member's closure — so the
//! owning set is computed here from the same graph the closure is taken over.
//!
//! The match is the crate-root prefix guppy itself reports, with the trailing
//! separator [`Workspace::crate_roots`] already carries for the reason it
//! carries it: `crates/aether-math/` must not claim `crates/aether-math-derive/`.
//! A path inside no workspace crate — a doc, a workflow, the lockfile — owns
//! nothing and is simply absent, which is the truthful answer rather than a
//! fallback to the whole workspace: what such a path can *break* is the
//! closure's question, and the closure fail-opens on it already.

use std::collections::BTreeSet;

use guppy::graph::PackageGraph;

/// The workspace packages `changed` writes into, by name.
pub(super) fn owning_packages(graph: &PackageGraph, changed: &[String]) -> BTreeSet<String> {
    let roots: Vec<(String, &str)> = graph
        .workspace()
        .iter()
        .filter_map(|package| {
            let root = package.source().workspace_path()?.as_str();
            (!root.is_empty()).then(|| (format!("{root}/"), package.name()))
        })
        .collect();

    changed
        .iter()
        .filter_map(|path| {
            roots.iter().find(|(prefix, _)| path.starts_with(prefix.as_str())).map(|(_, name)| (*name).to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use guppy::graph::PackageGraph;

    use super::owning_packages;

    fn graph() -> PackageGraph {
        guppy::MetadataCommand::new().build_graph().expect("build package graph")
    }

    #[test]
    fn a_changed_path_names_only_the_crate_that_contains_it() {
        // The plausible bug: a prefix match without the separator lets
        // `crates/aether-data/` claim `crates/aether-data-derive/`, which would
        // put a derive-crate edit into the data crate's written set and derive
        // an edge to every member that merely reads the data layer.
        let changed = ["crates/aether-data/src/lib.rs", "crates/aether-data-derive/src/lib.rs", "docs/guide/x.md"]
            .map(str::to_owned);
        let owners = owning_packages(&graph(), &changed);

        assert!(owners.contains("aether-data"), "the data path names the data crate: {owners:?}");
        assert!(owners.contains("aether-data-derive"), "the derive path names the derive crate: {owners:?}");
        assert_eq!(owners.len(), 2, "a path outside every crate root writes no package: {owners:?}");
    }
}
