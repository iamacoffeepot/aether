//! The native declared-dependency birth check (ADR-0230): refuse an actor
//! whose declared dependency has no `Live` route before `init` runs.
//!
//! The declaration is the actor's own [`Declared::Depends`] list (ADR-0231
//! §10), which `#[actor(depends(..))]` emits and a hand-written actor writes
//! itself, walked by [`declared_dependencies`]; the fold-and-`is_live` read is
//! the one [`Registry::missing_dependency`] both transports share.

use aether_actor::{Addressable, Declared, declared_dependencies};

use crate::chassis::error::BootError;
use crate::mail::registry::Registry;

/// Refuse `A`'s birth when a dependency its [`Declared::Depends`] lists has
/// no `Live` route, naming the actor and the missing namespace. Every
/// declarable dependency is a root singleton (ADR-0241 §5), so the read is
/// the same wherever `A` is placed.
pub fn check_declared<A: Addressable + Declared>(registry: &Registry) -> Result<(), BootError> {
    if let Some(namespace) = registry.missing_dependency(declared_dependencies::<A>()) {
        return Err(BootError::DependencyNotLive { actor: A::NAMESPACE, namespace });
    }
    Ok(())
}
