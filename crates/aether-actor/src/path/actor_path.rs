//! [`ActorPath<R>`]: an actor-typed path (ADR-0230 §2).

use alloc::format;
use core::marker::PhantomData;

use aether_data::{ActorPathError, ErasedActorPath, LoadName, Namespace};

use crate::{Addressable, ChildOf, Instanced, Root};

/// The canonical path of an `R`: an [`ErasedActorPath`] under the claim that
/// an `R` lives at the text (ADR-0230 §2).
///
/// Written here, the text is `R`'s canonical path, each step a type's
/// `NAMESPACE` and its key, by an actor that declares `links(R)`:
/// `ctx.link::<R>(&key)` for a root instance and
/// `ctx.link_child::<P, R>(&parent, &key)` for an instanced child. Decoded,
/// it claims only that the text is a well-formed canonical path, and `R` is
/// the writer's claim until `resolve` proves it. Nothing about existence
/// either way, and it grants no send.
///
/// On the wire and through serde it is the path text alone, with
/// [`ErasedActorPath`]'s schema and codec, so it may be a kind field, a
/// config field, or saved state. [`narrow`](Self::narrow) turns it into a
/// [`ProtocolPath<P>`](crate::ProtocolPath) for any protocol `R` covers.
pub struct ActorPath<R> {
    path: ErasedActorPath,
    _actor: PhantomData<fn() -> R>,
}

impl<R> ActorPath<R> {
    /// The typed view of `path`. The callers are the writers below and
    /// decode, so no crate can attach an `R` to arbitrary text (ADR-0230 §4).
    pub(crate) const fn from_erased(path: ErasedActorPath) -> Self {
        Self { path, _actor: PhantomData }
    }

    /// The text, for [`narrow`](Self::narrow), which keeps it.
    pub(crate) const fn erased(&self) -> &ErasedActorPath {
        &self.path
    }
}

impl<R: Root + Instanced> ActorPath<R> {
    /// `R::NAMESPACE:key`, the name the registry gives the root instance
    /// spawned under `key`. Reads no registry and folds nothing.
    ///
    /// Infallible by construction: the inline `const` makes an invalid
    /// `NAMESPACE` a compile error where this is monomorphized, and one step
    /// of two valid segments is at most 513 bytes at depth 1, under both caps.
    pub(crate) fn root_instance(key: &LoadName) -> Self {
        let _ = const { Namespace::new(R::NAMESPACE) };
        let text = format!("{}:{}", R::NAMESPACE, key.as_str());

        Self::from_erased(ErasedActorPath::new(&text).expect("one namespace and key step is a valid path"))
    }
}

impl<P: Addressable> ActorPath<P> {
    /// `<self>/C::NAMESPACE:key`, the name the registry gives the instanced
    /// `C` spawned under `key` beneath the actor at this path. Reads no
    /// registry and folds nothing.
    ///
    /// # Errors
    ///
    /// [`ErasedActorPath::new`]'s refusal, which here can only be the
    /// depth or byte cap: the parent is a valid canonical path, the inline
    /// `const` makes an invalid `C::NAMESPACE` a compile error, and the key
    /// is a valid segment.
    pub(crate) fn child_instance<C: ChildOf<P> + Instanced>(
        &self,
        key: &LoadName,
    ) -> Result<ActorPath<C>, ActorPathError> {
        let _ = const { Namespace::new(C::NAMESPACE) };
        let text = format!("{}/{}:{}", self.path, C::NAMESPACE, key.as_str());

        ErasedActorPath::new(&text).map(ActorPath::from_erased)
    }
}

typed_path_traits!(ActorPath<R>);
