//! What a construct lane's candidate wrote, in packages, and what that can
//! have broken.
//!
//! A bloom's members are grouped for verification today by bloom, composition
//! coverage, and construct base; nothing in that answers whether two members
//! can actually collide. The package graph does — a member whose edit lands in
//! a crate that another member's closure compiles is a member the other one can
//! be broken by, however disjoint their file lists — and the graph is already
//! computed twice per candidate: once by the construct lane's own lint bar and
//! once by the verify gate's scope. This records the pair that makes the
//! question answerable downstream: the packages the candidate *wrote* and the
//! reverse-dependency closure of that write.
//!
//! Recorded, not acted on. The evidence carries it, the coordinator's reads
//! serve it, and the console shows the edges it implies; the scheduler's own
//! use of it is a separate decision (ADR-0220).
//!
//! `closure: None` is the *unbounded* answer, exactly as
//! [`reverse_dependency_closure`](crate::affected::reverse_dependency_closure)
//! means it: the diff changed a workspace-level input, or the graph could not
//! be computed, and the gate will sweep the whole workspace. A reader that
//! renders unbounded as "depends on nothing" has it exactly backwards — a full
//! sweep compiles every sibling's write, so it is the one member that can be
//! broken by all of them.

use std::path::Path;

use crate::affected::graph::Workspace;
use crate::transform::fixers;
use crate::transform::verify::scope::Scope;

/// The packages a candidate wrote and the closure of that write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PackageClosure {
    /// The workspace packages the candidate's own paths land in, sorted.
    changed_packages: Vec<String>,
    /// The packages the write can have broken — itself plus every workspace
    /// package linking one of them. `None` when the blast radius is not bounded
    /// by the package graph at all.
    closure: Option<Vec<String>>,
    /// Why the closure is unbounded, in the gate's own words. `None` exactly
    /// when `closure` is `Some`, so the two are read as one answer.
    unbounded_reason: Option<String>,
}

impl PackageClosure {
    /// The record for the tree `worktree` currently holds — the construct
    /// lane's candidate, which is uncommitted, so the changed set is named from
    /// `git status` the way the lint bar names its own.
    pub(super) fn resolve(worktree: &Path, out_dir: &Path) -> Self {
        Self::of_changed(&fixers::dirty_paths(worktree, Some(out_dir)))
    }

    /// The record a stated changed set produces — the whole decision past the
    /// git read, so it is exercisable against a stated diff.
    fn of_changed(changed: &[String]) -> Self {
        let scope = Scope::of_changed(changed);
        let changed_packages =
            Workspace::load().map(|workspace| workspace.owning_packages(changed)).unwrap_or_default();

        Self {
            changed_packages: changed_packages.into_iter().collect(),
            closure: scope.packages().map(<[String]>::to_vec),
            unbounded_reason: match &scope {
                Scope::Workspace { reason } => Some(reason.clone()),
                Scope::Closure { .. } | Scope::Outside { .. } => None,
            },
        }
    }

    /// Stamp the record onto a construct evidence envelope.
    ///
    /// Always present, like the lint receipt and for the same reason: absence
    /// would be indistinguishable from a candidate that wrote nothing, and a
    /// reader deriving edges off that would report "this member depends on
    /// nobody" about a member nobody looked at.
    pub(super) fn stamp(self, evidence: &mut serde_json::Value) {
        if let Some(object) = evidence.as_object_mut() {
            object.insert(
                "package_closure".to_owned(),
                serde_json::json!({
                    "changed_packages": self.changed_packages,
                    "closure": self.closure,
                    "unbounded_reason": self.unbounded_reason,
                }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PackageClosure;

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn a_workspace_level_input_records_an_unbounded_closure_with_its_reason() {
        // The plausible bug this catches is the one that makes the whole
        // record misleading rather than merely absent: a diff the gate answers
        // with a full sweep recorded as a bounded closure of the crates it
        // happened to touch. A reader then derives that this member depends on
        // nothing, when it is the member that compiles every sibling's write.
        let record = PackageClosure::of_changed(&strings(&["xtask/src/transform/verify/mod.rs"]));

        assert!(record.closure.is_none(), "a gate-code change is not bounded by the package graph: {record:?}");
        assert!(
            record.unbounded_reason.as_deref().is_some_and(|reason| reason.contains("workspace-level input")),
            "the reason states which input widened it: {record:?}",
        );
    }

    #[test]
    fn a_crate_change_records_both_the_write_and_what_it_reaches() {
        // Tripwire: the two halves must stay distinct. `changed_packages` is
        // what the candidate wrote; `closure` is that plus its dependents.
        // Collapsing either into the other makes every member touching a
        // shared subtree read as depending on every other.
        let record = PackageClosure::of_changed(&strings(&["crates/aether-bloomery-git/src/lib.rs"]));

        assert_eq!(record.changed_packages, strings(&["aether-bloomery-git"]), "the write names one crate: {record:?}");
        let closure = record.closure.expect("a crate change is bounded by the graph");
        assert!(closure.contains(&"aether-bloomery-git".to_owned()), "the closure contains the write: {closure:?}");
        assert!(
            closure.len() > 1,
            "the git crate has workspace dependents, so the closure is wider than the write: {closure:?}"
        );
        assert!(record.unbounded_reason.is_none(), "a bounded closure states no widening reason");
    }
}
