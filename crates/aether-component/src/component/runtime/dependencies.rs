//! Declared-dependency refusal (ADR-0230): an actor whose `#[actor(depends(R))]`
//! entry has no `Live` route is refused before it is created — the load, the
//! module boot actor, or the replacement replies its operation's `Err`
//! naming the actor and the missing namespace, before `init` runs.

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
/// `None` when every entry is live. One derivation and one read for both
/// transports: the fold-and-`is_live` core lives on the registry as
/// [`Registry::missing_dependency`], and this is the component host's call
/// to it.
pub(super) fn missing_dependency<'a>(
    registry: &Registry,
    parent: MailboxId,
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
    // cannot prove an embedded peer live, so it folds from `NONE` and
    // refuses closed.
    let parent = canonical
        .rsplit_once('/')
        .and_then(|(path, _)| registry.resolve_address(path).ok())
        .map_or(MailboxId::NONE, |resolved| resolved.mailbox_id);
    missing_dependency(registry, parent, &group.dependencies).map(|namespace| dependency_refusal(&canonical, namespace))
}
