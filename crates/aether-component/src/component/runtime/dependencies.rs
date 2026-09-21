//! Declared-dependency refusal (ADR-0230): an actor whose `#[actor(depends(R))]`
//! entry has no `Live` route is refused before it is created — the load, the
//! module boot actor, or the replacement replies its operation's `Err`
//! naming the actor and the missing namespace, before `init` runs.

use aether_actor::{DependencyResolver, Embedded, One, Resolve};
use aether_substrate::actor::wasm::kind_manifest::{ActorInputs, Dependency};
use aether_substrate::mail::MailboxId;
use aether_substrate::mail::registry::Registry;

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
/// proceed. The target is the named export, or the entry group for a bare
/// replace; the parent is the replaced actor's own, read back from the
/// registry. An actor the registry does not know cannot swap, so there is
/// nothing to refuse and the replace proceeds down its existing path.
pub(super) fn replacement_refusal(
    registry: &Registry,
    actor_mailbox: MailboxId,
    actors: &[ActorInputs],
    export: Option<&str>,
) -> Option<String> {
    let canonical = registry.mailbox_name(actor_mailbox)?;
    // A bare replace reuses the trampoline's current hosted type, which the
    // host does not track; for a single-actor module the first group is that
    // type. An export the new module does not declare stays the trampoline's
    // error, as today.
    let group = match export {
        Some(requested) => actors.iter().find(|actor| actor.namespace.as_deref() == Some(requested))?,
        None => actors.first()?,
    };
    // The parent path is the canonical path minus its leaf (`/` is
    // structural — a subname cannot contain it), resolved through the
    // registry like any other address. A parent that no longer resolves, or
    // a root-placed actor with no parent path, cannot prove an embedded
    // peer live, so it folds from `NONE` and refuses closed.
    let parent = canonical
        .rsplit_once('/')
        .and_then(|(path, _)| registry.resolve_address(path).ok())
        .map_or(MailboxId::NONE, |resolved| resolved.mailbox_id);
    missing_dependency(registry, parent, &group.dependencies)
        .map(|namespace| format!("{canonical} depends on {namespace}, which is not live"))
}
