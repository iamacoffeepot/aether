//! The native declared-dependency birth check (ADR-0230): refuse an actor
//! whose `#[actor(depends(R))]` entry has no `Live` route before `init` runs.
//!
//! The declaration rides the link-time `DependencyEntry` inventory the
//! native `#[actor]` expansion populates; the fold-and-`is_live` read is the
//! one [`Registry::missing_dependency`] both transports share.

use aether_actor::Addressable;
use aether_data::name_inventory::dependency_entries;

use crate::chassis::error::BootError;
use crate::mail::MailboxId;
use crate::mail::registry::Registry;

/// Refuse `A`'s birth when one of its declared `DependencyEntry` entries
/// has no `Live` route, naming the actor and the missing namespace. A
/// root-pinned birth folds from the root (`MailboxId::NONE`); a spawned
/// child folds beneath its placement parent.
pub fn check_declared<A: Addressable>(registry: &Registry, parent: MailboxId) -> Result<(), BootError> {
    let missing = registry.missing_dependency(
        parent,
        dependency_entries().filter(|entry| entry.actor == A::NAMESPACE).map(|entry| (entry.resolver, entry.namespace)),
    );
    if let Some(namespace) = missing {
        return Err(BootError::DependencyNotLive { actor: A::NAMESPACE, namespace });
    }
    Ok(())
}
