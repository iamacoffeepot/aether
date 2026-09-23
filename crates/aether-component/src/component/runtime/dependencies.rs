//! Declared-dependency refusal (ADR-0230): an actor whose `#[actor(depends(R))]`
//! entry has no `Live` route is refused before it is created — the load, the
//! module boot actor, or the replacement replies its operation's `Err`
//! naming the actor and the missing namespace, before `init` runs. The
//! fourth site is the module itself (ADR-0230 §3): a load or replace also
//! refuses when an actor its module can spawn inline declares a dependency
//! with no `Live` route, before anything in the module runs.

use std::collections::HashSet;

use aether_data::{ActorLineageRecord, ActorPath};
use aether_substrate::actor::wasm::kind_manifest::{ActorInputs, Dependency};
use aether_substrate::mail::MailboxId;
use aether_substrate::mail::registry::Registry;

/// The refusal error naming the actor and its missing dependency. One
/// constructor for all four refusal sites, so the load, boot, and
/// replace paths cannot silently disagree on the wording.
pub(super) fn dependency_refusal(actor: &str, namespace: &str) -> String {
    format!("{actor} depends on {namespace}, which is not live")
}

/// The namespace of the first declared dependency with no `Live` route, or
/// `None` when every entry is live. One derivation and one read for both
/// transports: the fold-and-`is_live` core lives on the registry as
/// [`Registry::missing_dependency`], and this is the component host's call
/// to it.
pub(super) fn missing_dependency<'a>(
    registry: &Registry,
    parent: Option<MailboxId>,
    dependencies: &'a [Dependency],
) -> Option<&'a str> {
    registry.missing_dependency(parent, dependencies.iter().map(|d| (d.resolver, d.namespace.as_str())))
}

/// The refusal error for a replacement whose target actor declares a
/// dependency with no `Live` route, or `None` when the replacement may
/// proceed. The target is the named export, or the first non-boot group
/// for a bare replace; the parent is the replaced actor's own, read back
/// from the registry. An actor the registry does not know cannot swap, so
/// there is nothing to refuse and the replace proceeds down its existing
/// path.
pub(super) fn replacement_refusal(
    registry: &Registry,
    actor_mailbox: MailboxId,
    actors: &[ActorInputs],
    export: Option<&str>,
    boot: Option<&str>,
) -> Option<String> {
    let canonical = registry.mailbox_name(actor_mailbox)?;
    let group = match export {
        // An export the new module does not declare stays the trampoline's
        // error, as today.
        Some(requested) => actors.iter().find(|actor| actor.namespace.as_deref() == Some(requested))?,
        // A bare replace reuses the trampoline's current hosted type, which
        // the host does not track; the first group stands in, except the
        // boot group — the boot actor is never the hosted type, and its own
        // dependencies are checked separately, under the component host,
        // when the replacement module boot stages.
        None => actors.iter().find(|actor| boot.is_none_or(|ns| actor.namespace.as_deref() != Some(ns)))?,
    };
    // The parent path is the canonical path minus its leaf (`/` is
    // structural — a subname cannot contain it), resolved through the
    // registry like any other address. A parent that no longer resolves —
    // a live route is required, so a dead parent fails here even though the
    // child outlives it — or a root-placed actor with no parent path,
    // cannot prove an embedded peer live, so it passes no parent and
    // refuses closed.
    let parent = canonical
        .rsplit_once('/')
        .and_then(|(path, _)| ActorPath::new(path).ok())
        .and_then(|path| registry.resolve_address(&path).ok())
        .map(|resolved| resolved.mailbox_id);
    missing_dependency(registry, parent, &group.dependencies).map(|namespace| dependency_refusal(&canonical, namespace))
}

/// The module's inline-spawnable groups, in declaration order, each with its
/// resolved namespace: a group some actor of the same module can spawn inside
/// itself. The lineage section decides it — a `ModuleChild` record (what
/// `composable` emits), or a `Child` record whose parent is another exported
/// group of this module (what `child_of(P)` emits for an in-module `P`). A
/// `Root` record, or a `Child` under a parent outside the module, names no
/// in-module spawner. The implicit single-actor group (namespace `None`)
/// resolves through the module's `aether.namespace` section.
fn inline_spawnable<'a>(
    actors: &'a [ActorInputs],
    lineage: &[ActorLineageRecord],
    module_namespace: Option<&'a str>,
) -> impl Iterator<Item = (&'a str, &'a ActorInputs)> {
    let exported: HashSet<&str> =
        actors.iter().filter_map(|actor| actor.namespace.as_deref().or(module_namespace)).collect();
    let spawnable: HashSet<&str> = lineage
        .iter()
        .filter_map(|record| match record {
            ActorLineageRecord::ModuleChild { child_namespace, .. } => Some(child_namespace.as_ref()),
            ActorLineageRecord::Child { parent_namespace, child_namespace, .. } => {
                exported.contains(parent_namespace.as_ref()).then_some(child_namespace.as_ref())
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
/// load. The first such group, in declaration order, is refused with the
/// same wording as the other sites.
///
/// The check passes no parent, so a `One` entry folds from the root exactly
/// as for a component load, and an `Embedded` entry refuses closed: an
/// inline child's embedded peer folds beneath the child's own spawner,
/// which does not exist while its module loads, so nothing here can prove
/// it live.
pub(super) fn inline_dependency_refusal(
    registry: &Registry,
    actors: &[ActorInputs],
    lineage: &[ActorLineageRecord],
    module_namespace: Option<&str>,
) -> Option<String> {
    inline_spawnable(actors, lineage, module_namespace).find_map(|(namespace, group)| {
        missing_dependency(registry, None, &group.dependencies).map(|missing| dependency_refusal(namespace, missing))
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
        }
    }

    fn selected<'a>(
        actors: &'a [ActorInputs],
        lineage: &[ActorLineageRecord],
        module_namespace: Option<&'a str>,
    ) -> Vec<&'a str> {
        inline_spawnable(actors, lineage, module_namespace).map(|(namespace, _)| namespace).collect()
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
    fn matches_the_implicit_group_through_the_module_namespace() {
        let actors = [group(None)];
        let lineage = [ActorLineageRecord::ModuleChild { child: 1, child_namespace: "m.sole".into() }];

        assert_eq!(selected(&actors, &lineage, Some("m.sole")), ["m.sole"]);
        assert!(selected(&actors, &lineage, None).is_empty());
    }
}
