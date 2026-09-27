//! [`ProtocolPath<P>`]: a protocol-typed path (ADR-0231 §3), and the one way
//! to make one, [`ActorPath::narrow`].

use core::marker::PhantomData;

use aether_data::ErasedActorPath;
use aether_data::wire::{Error as WireError, WireDecode};
use serde::{Deserialize, Deserializer};

use super::ActorPath;
use crate::CoveredBy;

/// The canonical path of an actor whose rows cover the protocol `P`: an
/// [`ErasedActorPath`] under that claim (ADR-0231 §3).
///
/// Made only by [`ActorPath::narrow`], which compiles only when the
/// compiler has proved `P: CoveredBy<R>`. Decoded, it claims only that the
/// text is a well-formed canonical path, and `P` stays the writer's claim.
/// Nothing about existence either way, and it grants no send: a receiver's
/// `resolve` proves that a live actor stands at the path.
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

    /// The path text, for the native `resolve` in `aether-substrate`, which
    /// folds it. Grants nothing: the text is already public through
    /// `Display`, and a typed path cannot be built from it.
    #[must_use]
    pub const fn as_erased(&self) -> &ErasedActorPath {
        &self.path
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

/// A canonical path, or `WireError::InvalidActorPath`. The canonical check
/// alone: any actor covering `P` may live at the text, so no leaf is
/// compared.
impl<'de, P> WireDecode<'de> for ProtocolPath<P> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        super::decode_canonical(cursor).map(Self::from_erased)
    }
}

/// A canonical path, or a custom error naming the rule. The canonical check
/// alone, as the wire decode.
impl<'de, P> Deserialize<'de> for ProtocolPath<P> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        super::deserialize_canonical(deserializer).map(Self::from_erased)
    }
}

typed_path_traits!(ProtocolPath<P>);
