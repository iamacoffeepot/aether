//! Placement refusal (ADR-0241 §1, §5): a guest's placement comes from its
//! `#[actor]` declaration, exactly as a native type's does. A host `load`
//! and every module boot place a guest at the root, so each of those types
//! must declare `root`, which `#[actor(root)]` records as a `Root` lineage
//! record for every cardinality. A `load_under` places it beneath a live
//! parent, so its type must declare `child_of` the parent's type, a `Child`
//! record naming both. The checks run before the module publishes or
//! anything is staged: a load, or a replace whose module boot type has no
//! `Root`, is refused whole, and no route registers (#6821).
//!
//! A `ModuleChild` record satisfies neither; `composable` names an inline
//! spawn beneath any actor of the same module, which a guest reaches
//! instead.

use aether_data::ActorLineageRecord;

/// The placements `lineage` declares for `actor_namespace`, as a refusal
/// lists them.
fn declared_placements(lineage: &[ActorLineageRecord], actor_namespace: &str) -> String {
    let declared: Vec<String> = lineage
        .iter()
        .filter_map(|record| match record {
            ActorLineageRecord::Root { namespace, .. } if namespace == actor_namespace => Some("root".to_owned()),
            ActorLineageRecord::Child { parent_namespace, child_namespace, .. }
                if child_namespace == actor_namespace =>
            {
                Some(format!("child_of({parent_namespace})"))
            }
            ActorLineageRecord::ModuleChild { child_namespace, .. } if child_namespace == actor_namespace => {
                Some("composable".to_owned())
            }
            _ => None,
        })
        .collect();
    declared.join(", ")
}

/// The refusal error for placing `actor_namespace` at the root, or `None`
/// when its lineage carries a `Root` record for exactly that namespace. The
/// error names the actor, says a root placement needs `root`, and lists the
/// placements the lineage does declare for it.
pub(super) fn root_refusal(lineage: &[ActorLineageRecord], actor_namespace: &str) -> Option<String> {
    let rooted = lineage
        .iter()
        .any(|record| matches!(record, ActorLineageRecord::Root { namespace, .. } if namespace == actor_namespace));
    (!rooted).then(|| {
        format!(
            "{actor_namespace} cannot be placed at the root: a host load or module boot needs \
             `#[actor(root)]` (ADR-0241 §5); its declared placements are [{}]",
            declared_placements(lineage, actor_namespace)
        )
    })
}

/// The refusal error for placing `child_namespace` beneath a parent whose
/// type is `parent_namespace`, or `None` when its lineage carries a `Child`
/// record naming exactly that edge. The error names both, says a parented
/// placement needs `child_of` the parent, and lists the placements the
/// lineage does declare.
pub(super) fn child_refusal(
    lineage: &[ActorLineageRecord],
    child_namespace: &str,
    parent_namespace: &str,
) -> Option<String> {
    let edge = lineage.iter().any(|record| {
        matches!(
            record,
            ActorLineageRecord::Child { parent_namespace: parent, child_namespace: child, .. }
                if child == child_namespace && parent == parent_namespace
        )
    });
    (!edge).then(|| {
        format!(
            "{child_namespace} cannot be placed beneath {parent_namespace}: a load_under needs \
             `#[actor(child_of(..))]` naming the parent's type (ADR-0241 §5); its declared placements are [{}]",
            declared_placements(lineage, child_namespace)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(namespace: &'static str) -> ActorLineageRecord {
        ActorLineageRecord::Root { actor: 1, namespace: namespace.into() }
    }

    fn child(parent: &'static str, namespace: &'static str) -> ActorLineageRecord {
        ActorLineageRecord::Child {
            parent: 1,
            child: 2,
            parent_namespace: parent.into(),
            child_namespace: namespace.into(),
        }
    }

    fn module_child(namespace: &'static str) -> ActorLineageRecord {
        ActorLineageRecord::ModuleChild { child: 2, child_namespace: namespace.into() }
    }

    #[test]
    fn admits_only_a_root_record_for_the_same_namespace() {
        let cases: [(&str, Vec<ActorLineageRecord>, bool); 6] = [
            ("root for the namespace", vec![root("m.actor")], true),
            ("root beside a child record", vec![child("m.parent", "m.actor"), root("m.actor")], true),
            ("root for another namespace", vec![root("m.other")], false),
            ("child record alone", vec![child("m.parent", "m.actor")], false),
            ("module child record alone", vec![module_child("m.actor")], false),
            ("no records", Vec::new(), false),
        ];

        for (case, lineage, admitted) in cases {
            assert_eq!(root_refusal(&lineage, "m.actor").is_none(), admitted, "{case}");
        }
    }

    // Catches: a load_under admitted by any declared edge rather than one
    // naming the proven parent's type, so a guest lands beneath a parent its
    // type never declared (#6821).
    #[test]
    fn a_child_placement_admits_only_the_edge_to_that_parent() {
        let cases: [(&str, Vec<ActorLineageRecord>, bool); 5] = [
            ("the edge to the parent", vec![child("m.parent", "m.actor")], true),
            ("the edge beside a root record", vec![root("m.actor"), child("m.parent", "m.actor")], true),
            ("an edge to another parent", vec![child("m.other", "m.actor")], false),
            ("a root record alone", vec![root("m.actor")], false),
            ("a module child record alone", vec![module_child("m.actor")], false),
        ];

        for (case, lineage, admitted) in cases {
            assert_eq!(child_refusal(&lineage, "m.actor", "m.parent").is_none(), admitted, "{case}");
        }
    }

    #[test]
    fn a_child_refusal_names_both_actors_and_the_declared_placements() {
        let lineage = [root("m.actor"), child("m.other", "m.actor")];

        let error = child_refusal(&lineage, "m.actor", "m.parent").expect("the edge to m.parent is undeclared");

        assert!(error.starts_with("m.actor cannot be placed beneath m.parent"), "{error}");
        assert!(error.ends_with("[root, child_of(m.other)]"), "{error}");
    }

    #[test]
    fn the_refusal_names_the_actor_and_its_declared_placements() {
        let lineage = [root("m.other"), child("m.parent", "m.actor"), module_child("m.actor")];

        let error = root_refusal(&lineage, "m.actor").expect("a child-only type is refused");

        assert!(error.starts_with("m.actor cannot be placed"), "{error}");
        assert!(error.contains("root"), "{error}");
        assert!(error.ends_with("[child_of(m.parent), composable]"), "{error}");
    }
}
