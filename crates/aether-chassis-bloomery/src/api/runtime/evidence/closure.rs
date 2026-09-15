//! The package closures a bloom's members recorded, and the edges they imply.
//!
//! The construct lane stamps each candidate's written packages and their
//! reverse-dependency closure into its `evidence.json`. That file is already
//! addressable from the member's dispatch row, so this reads it there rather
//! than persisting the value into the journal graph: the closure is a property
//! of one attempt's tree, and the edge is a fact about a pair of attempts,
//! neither of which the reducer owns or replays.
//!
//! An edge runs from the member whose closure *reaches* a package to the member
//! that *wrote* it — `A depends_on B` means B's change lands in a crate A's
//! verification compiles. The direction matters: it is not symmetric, and it is
//! what says which of the two a red run cannot exonerate by file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use aether_bloomery::{PackageClosureView, SemanticEdgeView};

use super::{evidence_retained, resolve_evidence_dir};

/// The file inside a dispatch's evidence directory that carries the record.
const EVIDENCE_FILE: &str = "evidence.json";

/// The record one dispatch's evidence carries, or `None` when the directory is
/// swept, the file is unreadable, or the attempt predates the lane recording it.
///
/// Tolerant on the way in for the same reason every other evidence read here is:
/// a row whose closure cannot be read is a row without one, never a failed
/// response over the whole bloom.
pub fn read(worktree_base: &Path, archive_base: &Path, nonce: &str) -> Option<PackageClosureView> {
    if !evidence_retained(worktree_base, archive_base, nonce) {
        return None;
    }

    serde_json::from_slice::<serde_json::Value>(
        &fs::read(resolve_evidence_dir(worktree_base, archive_base, nonce)?.join(EVIDENCE_FILE)).ok()?,
    )
    .ok()?
    .get("package_closure")
    .and_then(|record| serde_json::from_value(record.clone()).ok())
}

/// The semantic edges a set of `(member, closure)` pairs implies.
///
/// A member whose closure is unbounded reaches every package in the workspace,
/// so it depends on every sibling that wrote one — the case a reader must not
/// render as "depends on nothing", because a full sweep is precisely the run
/// every sibling's change can turn red. Its `through` names the packages the
/// sibling wrote, which is what the sweep will compile.
///
/// A member never depends on itself, and a pair is reported at most once per
/// direction with every package the edge runs through.
pub fn derive_edges(members: &[(String, PackageClosureView)]) -> Vec<SemanticEdgeView> {
    let mut edges: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    for (member, record) in members {
        for (peer, peer_record) in members.iter().filter(|(peer, _)| peer != member) {
            let through: BTreeSet<&str> = peer_record
                .changed_packages
                .iter()
                .map(String::as_str)
                .filter(|package| {
                    record.closure.as_ref().is_none_or(|closure| closure.iter().any(|in_closure| in_closure == package))
                })
                .collect();
            if !through.is_empty() {
                edges.entry((member.as_str(), peer.as_str())).or_default().extend(through);
            }
        }
    }

    edges
        .into_iter()
        .map(|((member, depends_on), through)| SemanticEdgeView {
            member: member.to_owned(),
            depends_on: depends_on.to_owned(),
            through: through.into_iter().map(str::to_owned).collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use aether_bloomery::PackageClosureView;

    use super::derive_edges;

    fn record(changed: &[&str], closure: Option<&[&str]>) -> PackageClosureView {
        PackageClosureView {
            changed_packages: changed.iter().map(|name| (*name).to_owned()).collect(),
            closure: closure.map(|names| names.iter().map(|name| (*name).to_owned()).collect()),
            unbounded_reason: closure.is_none().then(|| "a workspace-level input changed".to_owned()),
        }
    }

    // The plausible bug: an edge is derived from closure overlap rather than
    // from one member's write landing in the other's closure. Two members that
    // both merely *depend on* the same crate would then read as depending on
    // each other, and the grouping this feeds would refuse to coalesce every
    // pair in the workspace.
    #[test]
    fn an_edge_runs_from_the_closure_that_reaches_a_write_to_its_author() {
        let members = vec![
            (
                "consumer".to_owned(),
                record(&["aether-chassis-bloomery"], Some(&["aether-chassis-bloomery", "aether-bloomery"])),
            ),
            ("author".to_owned(), record(&["aether-bloomery"], Some(&["aether-bloomery", "aether-chassis-bloomery"]))),
        ];
        let edges = derive_edges(&members);

        assert_eq!(edges.len(), 2, "each member's closure reaches the other's write: {edges:?}");
        let forward = edges.iter().find(|edge| edge.member == "consumer").expect("the consumer's edge");
        assert_eq!(forward.depends_on, "author");
        assert_eq!(forward.through, vec!["aether-bloomery".to_owned()], "the edge names the package it runs through");
    }

    // The bug this exists for, stated by the owner: a member whose closure is
    // unbounded — the whole-workspace sweep — must not be shown as depending on
    // nothing. It is the member every sibling's change compiles into.
    #[test]
    fn an_unbounded_closure_depends_on_every_sibling_that_wrote_a_package() {
        let members = vec![
            ("sweeper".to_owned(), record(&["xtask"], None)),
            ("author".to_owned(), record(&["aether-bloomery"], Some(&["aether-bloomery"]))),
            ("doc-only".to_owned(), record(&[], Some(&[]))),
        ];
        let edges = derive_edges(&members);

        let sweeper: Vec<&str> =
            edges.iter().filter(|edge| edge.member == "sweeper").map(|edge| edge.depends_on.as_str()).collect();
        assert_eq!(sweeper, vec!["author"], "the sweep depends on every sibling that wrote a package: {edges:?}");
        assert!(
            !edges.iter().any(|edge| edge.member == "author" && edge.depends_on == "sweeper"),
            "a bounded closure that does not hold xtask is not reached by the sweeper's write: {edges:?}",
        );
    }

    #[test]
    fn a_member_never_depends_on_itself() {
        let members = vec![("solo".to_owned(), record(&["aether-math"], Some(&["aether-math"])))];
        assert!(derive_edges(&members).is_empty(), "a lone member has no peer to depend on");
    }
}
