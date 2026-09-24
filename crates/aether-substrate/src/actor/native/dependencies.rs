//! The native declared-dependency birth check (ADR-0230): refuse an actor
//! whose `#[actor(depends(R))]` entry has no `Live` route before `init` runs.
//!
//! The declaration rides the link-time `DependencyEntry` inventory the
//! native `#[actor]` expansion populates, read through a per-actor index
//! folded once per process; the fold-and-`is_live` read is the one
//! [`Registry::missing_dependency`] both transports share.

use std::collections::HashMap;
use std::sync::OnceLock;

use aether_actor::Addressable;
use aether_data::name_inventory::dependency_entries;

use crate::chassis::error::BootError;
use crate::mail::MailboxId;
use crate::mail::registry::Registry;

/// Refuse `A`'s birth when one of its declared `DependencyEntry` entries
/// has no `Live` route, naming the actor and the missing namespace. A
/// root-pinned birth has no parent (`None`); a spawned child folds beneath
/// its placement parent.
pub fn check_declared<A: Addressable>(registry: &Registry, parent: Option<MailboxId>) -> Result<(), BootError> {
    let missing =
        registry.missing_dependency(parent, declared_by_actor().get(A::NAMESPACE).into_iter().flatten().copied());
    if let Some(namespace) = missing {
        return Err(BootError::DependencyNotLive { actor: A::NAMESPACE, namespace });
    }
    Ok(())
}

/// Every native actor's declared dependencies, keyed by the declaring
/// actor's `NAMESPACE`, folded once from the link-time inventory.
fn declared_by_actor() -> &'static HashMap<&'static str, Vec<(u8, &'static str)>> {
    static INDEX: OnceLock<HashMap<&'static str, Vec<(u8, &'static str)>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut index: HashMap<&'static str, Vec<(u8, &'static str)>> = HashMap::new();
        for entry in dependency_entries() {
            index.entry(entry.actor).or_default().push((entry.resolver, entry.namespace));
        }
        index
    })
}
