//! Declared-dependency refusal (ADR-0230): an actor whose `#[actor(depends(R))]`
//! entry has no `Live` route is refused before it is created — the load, the
//! module boot actor, or the republish replies its operation's `Err` naming
//! the actor and the missing namespace, before `init` runs. A republish
//! checks each live member for a dependency its successor type adds, and
//! names every member it refuses (ADR-0241 §4). The fourth site is the
//! module itself (ADR-0230 §3): a load or replace also refuses when an
//! actor its module can spawn inline declares a dependency with no `Live`
//! route, before anything in the module runs. Those actors are the exported
//! inline-spawnable types and every private inline child (issue 6590), read
//! from the module's `aether.kinds.inputs.private` section.
//!
//! Every site asks one read, the ctx's `missing_dependency`, whose
//! fold-and-liveness core is the registry's own for both transports. Every
//! declarable dependency is a root singleton (ADR-0241 §5), so the read takes
//! no placement.

use std::collections::HashSet;

use aether_actor::ReplyMode;
use aether_data::ActorLineageRecord;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::actor::wasm::kind_manifest::{ActorInputs, Dependency};
use aether_substrate::actor::wasm::module::ModuleManifest;

/// The refusal error naming the actor and its missing dependency. One
/// constructor for all four refusal sites, so the load, boot, and
/// replace paths cannot silently disagree on the wording.
pub(super) fn dependency_refusal(actor: &str, namespace: &str) -> String {
    format!("{actor} depends on {namespace}, which is not live")
}

/// The refusal error for a republish one of whose live members' successor
/// type adds a dependency with no `Live` route, naming every such member by
/// its path, or `None` when every member may prepare (ADR-0241 §4). Each
/// member is its path, its type's dependencies before the republish, and
/// after. A dependency the type already declared was live when the member
/// was created, so only an added one is checked.
pub(super) fn replacement_refusal<'a, A, M: ReplyMode>(
    ctx: &NativeCtx<'_, A, M>,
    members: impl IntoIterator<Item = (&'a str, &'a [Dependency], &'a [Dependency])>,
) -> Option<String> {
    let refused: Vec<String> = members
        .into_iter()
        .filter_map(|(path, before, after)| {
            let added: Vec<Dependency> = after
                .iter()
                .filter(|dependency| before.iter().all(|kept| kept.namespace != dependency.namespace))
                .cloned()
                .collect();
            ctx.missing_dependency(&added).map(|namespace| dependency_refusal(path, namespace))
        })
        .collect();
    (!refused.is_empty()).then(|| refused.join("; "))
}

/// The module's inline-spawnable exported groups, in declaration order, each
/// with its resolved namespace: a group some actor of the same module can
/// spawn inside itself. The lineage section decides it — a `ModuleChild`
/// record (what `composable` emits), or a `Child` record whose parent is
/// another exported group or a private group of this module (what
/// `child_of(P)` emits for an in-module `P`). A `Root` record, or a `Child`
/// under a parent outside the module, names no in-module spawner. The
/// implicit single-actor group (namespace `None`) resolves through the
/// module's `aether.namespace` section. The private groups themselves need no
/// selection: a private type exists to be spawned inline.
fn inline_spawnable<'a>(
    actors: &'a [ActorInputs],
    private: &[ActorInputs],
    lineage: &[ActorLineageRecord],
    module_namespace: Option<&'a str>,
) -> impl Iterator<Item = (&'a str, &'a ActorInputs)> {
    let in_module: HashSet<&str> = actors
        .iter()
        .filter_map(|actor| actor.namespace.as_deref().or(module_namespace))
        .chain(private.iter().filter_map(|actor| actor.namespace.as_deref()))
        .collect();
    let spawnable: HashSet<&str> = lineage
        .iter()
        .filter_map(|record| match record {
            ActorLineageRecord::ModuleChild { child_namespace, .. } => Some(child_namespace.as_ref()),
            ActorLineageRecord::Child { parent_namespace, child_namespace, .. } => {
                in_module.contains(parent_namespace.as_ref()).then_some(child_namespace.as_ref())
            }
            ActorLineageRecord::Root { .. } => None,
        })
        .collect();
    actors.iter().filter_map(move |actor| {
        let namespace = actor.namespace.as_deref().or(module_namespace)?;
        spawnable.contains(namespace).then_some((namespace, actor))
    })
}

/// The refusal error for a module one of whose inline-spawnable actors
/// declares a dependency with no `Live` route, or `None` when the module may
/// load. The selected exported groups are checked first, in declaration
/// order, then every private group, and the first failing group is refused
/// with the same wording as the other sites. A private group needs no
/// lineage filter: every private type is an inline child.
///
/// Every entry is a root singleton, so it folds from the root exactly as
/// for a component load.
pub(super) fn inline_dependency_refusal<A, M: ReplyMode>(
    ctx: &NativeCtx<'_, A, M>,
    manifest: &ModuleManifest,
) -> Option<String> {
    let private = manifest.private_actors();
    let private_groups = private.iter().filter_map(|group| Some((group.namespace.as_deref()?, group)));
    inline_spawnable(manifest.actors(), private, manifest.lineage(), manifest.namespace())
        .chain(private_groups)
        .find_map(|(namespace, group)| {
            ctx.missing_dependency(&group.dependencies).map(|missing| dependency_refusal(namespace, missing))
        })
}

#[cfg(test)]
mod tests {
    use aether_kinds::ComponentCapabilities;

    use super::*;

    fn group(namespace: Option<&str>) -> ActorInputs {
        ActorInputs {
            namespace: namespace.map(str::to_owned),
            capabilities: ComponentCapabilities::default(),
            dependencies: Vec::new(),
            instanced: false,
        }
    }

    fn selected<'a>(
        actors: &'a [ActorInputs],
        lineage: &[ActorLineageRecord],
        module_namespace: Option<&'a str>,
    ) -> Vec<&'a str> {
        selected_with_private(actors, &[], lineage, module_namespace)
    }

    fn selected_with_private<'a>(
        actors: &'a [ActorInputs],
        private: &[ActorInputs],
        lineage: &[ActorLineageRecord],
        module_namespace: Option<&'a str>,
    ) -> Vec<&'a str> {
        inline_spawnable(actors, private, lineage, module_namespace).map(|(namespace, _)| namespace).collect()
    }

    #[test]
    fn selects_module_children_and_children_of_an_exported_parent() {
        let actors = [group(Some("m.parent")), group(Some("m.composable")), group(Some("m.child"))];
        let lineage = [
            ActorLineageRecord::Root { actor: 1, namespace: "m.parent".into() },
            ActorLineageRecord::ModuleChild { child: 2, child_namespace: "m.composable".into() },
            ActorLineageRecord::Child {
                parent: 1,
                child: 3,
                parent_namespace: "m.parent".into(),
                child_namespace: "m.child".into(),
            },
        ];

        assert_eq!(selected(&actors, &lineage, None), ["m.composable", "m.child"]);
    }

    #[test]
    fn skips_roots_and_children_of_a_parent_outside_the_module() {
        let actors = [group(Some("m.root")), group(Some("m.foreign_child"))];
        let lineage = [
            ActorLineageRecord::Root { actor: 1, namespace: "m.root".into() },
            ActorLineageRecord::Child {
                parent: 9,
                child: 2,
                parent_namespace: "elsewhere.parent".into(),
                child_namespace: "m.foreign_child".into(),
            },
        ];

        assert!(selected(&actors, &lineage, None).is_empty());
    }

    #[test]
    fn selects_children_of_a_private_parent() {
        let actors = [group(Some("m.child"))];
        let private = [group(Some("m.private_parent"))];
        let lineage = [ActorLineageRecord::Child {
            parent: 1,
            child: 2,
            parent_namespace: "m.private_parent".into(),
            child_namespace: "m.child".into(),
        }];

        assert_eq!(selected_with_private(&actors, &private, &lineage, None), ["m.child"]);
    }

    #[test]
    fn matches_the_implicit_group_through_the_module_namespace() {
        let actors = [group(None)];
        let lineage = [ActorLineageRecord::ModuleChild { child: 1, child_namespace: "m.sole".into() }];

        assert_eq!(selected(&actors, &lineage, Some("m.sole")), ["m.sole"]);
        assert!(selected(&actors, &lineage, None).is_empty());
    }
}
