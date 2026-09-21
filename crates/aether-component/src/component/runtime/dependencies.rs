//! Declared-dependency refusal (ADR-0230): an actor whose `#[actor(depends(R))]`
//! entry has no `Live` route is refused before it is created — the load, the
//! module boot actor, or the replacement replies its operation's `Err`
//! naming the actor and the missing namespace, before `init` runs.

use aether_actor::{DependencyResolver, Embedded, One, Resolve};
use aether_substrate::actor::wasm::kind_manifest::{ActorInputs, Dependency};
use aether_substrate::mail::MailboxId;
use aether_substrate::mail::registry::Registry;

/// The refusal error naming the actor and its missing dependency. One
/// constructor for all three refusal sites, so the load, boot, and
/// replace paths cannot silently disagree on the wording.
pub(super) fn dependency_refusal(actor: &str, namespace: &str) -> String {
    format!("{actor} depends on {namespace}, which is not live")
}

/// The namespace of the first declared dependency with no `Live` route, or
/// `None` when every entry is live. Each entry folds to its position
/// through its own strategy — [`One`] at the root, [`Embedded`] beneath
/// the placement's `parent` — and the registry answers whether a `Live`
/// route is there. A missing dependency is a refusal and nothing else: no
/// ordering, no retry, no wait.
pub(super) fn missing_dependency<'a>(
    registry: &Registry,
    parent: MailboxId,
    dependencies: &'a [Dependency],
) -> Option<&'a str> {
    dependencies.iter().find_map(|dependency| {
        let candidate = match dependency.resolver {
            One::TAG => One::candidate(MailboxId::NONE.0, &dependency.namespace, None),
            Embedded::TAG => Embedded::candidate(parent.0, &dependency.namespace, None),
            // The inputs reader rejects a tag no strategy claims, so an
            // unknown tag never arrives here; refuse closed anyway.
            _ => return Some(dependency.namespace.as_str()),
        };
        match candidate {
            Some(id) if registry.is_live(id) => None,
            _ => Some(dependency.namespace.as_str()),
        }
    })
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
    // cannot prove an embedded peer live, so it folds from `NONE` and
    // refuses closed.
    let parent = canonical
        .rsplit_once('/')
        .and_then(|(path, _)| registry.resolve_address(path).ok())
        .map_or(MailboxId::NONE, |resolved| resolved.mailbox_id);
    missing_dependency(registry, parent, &group.dependencies).map(|namespace| dependency_refusal(&canonical, namespace))
}

#[cfg(test)]
mod tests {
    use std::slice::from_ref;

    use aether_substrate::mail::registry::noop_handler;
    use aether_substrate::testing::boot_authority;

    use super::*;

    #[test]
    fn dependency_folds_match_registered_positions() {
        let registry = Registry::new();
        let authority = boot_authority();
        registry.register_inbox(&authority, "test.dependency.one", noop_handler());
        let parent = registry.register_inbox(&authority, "test.dependency.parent", noop_handler());
        let elsewhere = registry.register_inbox(&authority, "test.dependency.elsewhere", noop_handler());
        // The spawn path registers nested actors under a caller-folded id;
        // the test folds the peer's the same way `missing_dependency` does.
        // (The harness scenario pins the fold against the real spawn path.)
        let Some(peer_id) = Embedded::candidate(parent.0, "test.dependency.peer", None) else {
            panic!("keyless candidate is Some");
        };
        assert!(
            registry
                .try_register_inbox_with_id(
                    &authority,
                    peer_id,
                    "test.dependency.parent/aether.embedded:test.dependency.peer",
                    noop_handler(),
                )
                .is_ok()
        );

        // A `One` dependency folds from the root, so the registered root
        // name satisfies it under any placement parent, including `NONE`.
        let one = Dependency { resolver: One::TAG, namespace: "test.dependency.one".to_owned() };
        assert_eq!(missing_dependency(&registry, parent, from_ref(&one)), None);
        assert_eq!(missing_dependency(&registry, MailboxId::NONE, from_ref(&one)), None);

        // An `Embedded` dependency folds beneath the placement's parent —
        // and only there.
        let peer = Dependency { resolver: Embedded::TAG, namespace: "test.dependency.peer".to_owned() };
        assert_eq!(missing_dependency(&registry, parent, from_ref(&peer)), None);
        assert_eq!(missing_dependency(&registry, elsewhere, from_ref(&peer)), Some("test.dependency.peer"));

        // An absent namespace is missing, and an unknown tag refuses closed
        // even when its namespace is live.
        let absent = Dependency { resolver: One::TAG, namespace: "test.dependency.absent".to_owned() };
        assert_eq!(missing_dependency(&registry, parent, &[absent]), Some("test.dependency.absent"));
        let unknown = Dependency { resolver: 0xFF, namespace: "test.dependency.one".to_owned() };
        assert_eq!(missing_dependency(&registry, parent, &[unknown]), Some("test.dependency.one"));

        assert_eq!(missing_dependency(&registry, parent, &[]), None);
    }
}
