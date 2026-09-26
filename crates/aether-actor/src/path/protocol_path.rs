//! [`ProtocolPath<P>`]: a protocol-typed path (ADR-0231 §3), and the one way
//! to make one, [`ActorPath::narrow`].

use core::marker::PhantomData;

use aether_data::ErasedActorPath;

use super::ActorPath;
use crate::CoveredBy;

/// The canonical path of an actor whose rows cover the protocol `P`: an
/// [`ErasedActorPath`] under that claim (ADR-0231 §3).
///
/// Made only by [`ActorPath::narrow`], which compiles only when the
/// compiler has proved `P: CoveredBy<R>`. Decoded, it claims only that the
/// text is a well-formed canonical path, and `P` is the writer's claim until
/// `resolve` proves it against the route's published rows. Nothing about
/// existence either way, and it grants no send.
///
/// On the wire and through serde it is the path text alone, with
/// [`ErasedActorPath`]'s schema and codec, so it may be a kind field, a
/// config field, or saved state.
pub struct ProtocolPath<P> {
    path: ErasedActorPath,
    _protocol: PhantomData<fn() -> P>,
}

impl<P> ProtocolPath<P> {
    /// The typed view of `path`. The callers are [`ActorPath::narrow`] and
    /// decode, so no crate can attach a `P` to arbitrary text (ADR-0230 §4).
    pub(crate) const fn from_erased(path: ErasedActorPath) -> Self {
        Self { path, _protocol: PhantomData }
    }
}

impl<R> ActorPath<R> {
    /// The same text under a narrower claim: an actor covering `P` lives
    /// here. Compiles only for `P: CoveredBy<R>`, the sealed coverage check
    /// over `R`'s contract rows (ADR-0231 §2).
    ///
    /// Keeps the text the actor path was written with, so the protocol path
    /// is canonical too. Reads no registry, folds nothing, and cannot fail.
    #[must_use]
    pub fn narrow<P: CoveredBy<R>>(&self) -> ProtocolPath<P> {
        ProtocolPath::from_erased(self.erased().clone())
    }
}

typed_path_traits!(ProtocolPath<P>);
