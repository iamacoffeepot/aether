//! Host-placement refusal (ADR-0241 §1, §5): a guest's placement comes from
//! its `#[actor]` declaration, exactly as a native type's does. The component
//! host places an actor at itself for a host `load` and for every module
//! boot, so each of those types must declare `root`, which `#[actor(root)]`
//! records as a `Root` lineage record for every cardinality. The check runs
//! before the module publishes or anything is staged: a load, or a replace
//! whose module boot type has no `Root`, is refused whole.
//!
//! A `Child` or `ModuleChild` record never satisfies it; those name a parent
//! placement, which a `load_under` or an inline spawn reaches instead.

use aether_data::ActorLineageRecord;

/// The refusal error for placing `actor_namespace` at the component host, or
/// `None` when its lineage carries a `Root` record for exactly that
/// namespace. The error names the actor, says a host placement needs `root`,
/// and lists the placements the lineage does declare for it.
pub(super) fn root_refusal(lineage: &[ActorLineageRecord], actor_namespace: &str) -> Option<String> {
    let mut declared = Vec::new();
    for record in lineage {
        match record {
            ActorLineageRecord::Root { namespace, .. } if namespace == actor_namespace => return None,
            ActorLineageRecord::Child { parent_namespace, child_namespace, .. }
                if child_namespace == actor_namespace =>
            {
                declared.push(format!("child_of({parent_namespace})"));
            }
            ActorLineageRecord::ModuleChild { child_namespace, .. } if child_namespace == actor_namespace => {
                declared.push("composable".to_owned());
            }
            _ => {}
        }
    }
    Some(format!(
        "{actor_namespace} cannot be placed at the component host: a host load or module boot needs \
         `#[actor(root)]` (ADR-0241 §5); its declared placements are [{}]",
        declared.join(", ")
    ))
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

    #[test]
    fn the_refusal_names_the_actor_and_its_declared_placements() {
        let lineage = [root("m.other"), child("m.parent", "m.actor"), module_child("m.actor")];

        let error = root_refusal(&lineage, "m.actor").expect("a child-only type is refused");

        assert!(error.starts_with("m.actor cannot be placed"), "{error}");
        assert!(error.contains("root"), "{error}");
        assert!(error.ends_with("[child_of(m.parent), composable]"), "{error}");
    }
}
