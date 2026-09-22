//! Typed [`Address`] constructors (ADR-0230).
//!
//! The position and the proof have different owners. This module owns the
//! description: the typed `Address<R>` that names where an actor of type `R`
//! would sit. Whether anything is `Live` there is the host's answer, and only
//! resolving against the registry turns a description into a reference.

use aether_data::{Address, LoadName};

use super::{Addressable, ChildOf, Embedded, Instanced, Singleton};
use crate::reference::ActorRef;

/// The address of the singleton `R`: resolved relative to the scope `R`'s
/// resolver selects from the caller, with no discriminator.
#[must_use]
pub fn address<R: Singleton>() -> Address<R> {
    Address::scoped(None)
}

/// The address of the embedded `R` its host loaded under `name`: a carried
/// load name folds in the namespace slot of the same fold [`address`] performs.
#[must_use]
pub fn address_named<R: Addressable<Resolver = Embedded>>(name: LoadName) -> Address<R> {
    Address::scoped(Some(name))
}

/// The address of the `R` instance keyed by `key`: resolved relative to the
/// scope `R`'s resolver selects from the caller.
#[must_use]
pub fn address_at<R: Instanced>(key: LoadName) -> Address<R> {
    Address::scoped(Some(key))
}

/// The address of the `C` instance keyed by `key` directly beneath `parent`.
/// The parent travels as a proven [`ActorRef`], so a child address can only
/// be built beneath an actor that reached `Live`.
#[must_use]
pub fn child_address<P: Addressable, C: ChildOf<P> + Instanced>(parent: ActorRef<P>, key: LoadName) -> Address<C> {
    Address::beneath(parent.id(), Some(key))
}
