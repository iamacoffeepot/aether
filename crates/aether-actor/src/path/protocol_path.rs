//! [`ProtocolPath<P>`]: a protocol-typed path (ADR-0231 §3), made by
//! [`ActorPath::narrow`] or decoded against the engine's published route
//! contracts.

use core::marker::PhantomData;

use aether_data::ErasedActorPath;
use aether_data::wire::{Decoder, Error as WireError, WireDecode};

use super::ActorPath;
use crate::{CoveredBy, Protocol, RowSet};

/// The canonical path of an actor whose rows cover the protocol `P`: an
/// [`ErasedActorPath`] under that claim (ADR-0231 §3).
///
/// A value that exists holds its claim, made one of two ways:
///
/// - [`ActorPath::narrow`], which compiles only when the compiler has
///   proved `P: CoveredBy<R>`;
/// - a decode through [`Kind::decode_with`](aether_data::Kind::decode_with),
///   whose context proves that the `Live` or `Dropped` route standing at
///   the path published every row of `P` (ADR-0231 §4). The decode refuses
///   [`WireError::ProtocolPathUnchecked`] when the context has no published
///   routes, [`WireError::ProtocolPathUnpublished`] when no route has stood
///   at the path or it is still starting, and
///   [`WireError::UncoveredProtocolPath`] naming the first row of `P` the
///   route does not publish. The plain shorthand (`decode_from_bytes`,
///   `wire::decode_from_slice`) decodes with an empty context, so it always
///   refuses.
///
/// It proves type, not liveness, and grants no send. A path whose actor has
/// closed still decodes, since names are never reused, and the route can
/// leave between decode and use either way, so a receiver's `resolve`
/// proves that a live actor still stands at the path and its handler
/// answers the "not live" case itself. A refused decode reaches no handler:
/// the mail is dropped with a warn and nothing is sent back.
///
/// On the wire it is the path text alone, with [`ErasedActorPath`]'s schema
/// and codec, so it may be a kind field. It serializes but has no
/// `Deserialize`, since serde carries no context: a kind carrying one is
/// declared `no_serde`.
///
/// ```compile_fail,E0277
/// use aether_actor::{ProtocolPath, protocol};
///
/// #[aether_data::kind(name = "example.ping")]
/// struct Ping {
///     seq: u32,
/// }
///
/// #[protocol]
/// trait Pinging {
///     fn ping(mail: Ping);
/// }
///
/// // The kind derives `Deserialize`, which `ProtocolPath` does not implement.
/// #[aether_data::kind(name = "example.carries")]
/// struct Carries {
///     path: ProtocolPath<Pinging>,
/// }
///
/// fn main() {}
/// ```
///
/// Declared `no_serde`, the same kind decodes through `decode_with` only:
///
/// ```
/// use aether_actor::{Kind, ProtocolPath, protocol};
/// use aether_data::ErasedActorPath;
/// use aether_data::wire::{self, DecodeCtx, Error};
///
/// #[aether_data::kind(name = "example.ping")]
/// struct Ping {
///     seq: u32,
/// }
///
/// #[protocol]
/// trait Pinging {
///     fn ping(mail: Ping);
/// }
///
/// #[aether_data::kind(name = "example.carries", no_serde)]
/// struct Carries {
///     path: ProtocolPath<Pinging>,
/// }
///
/// fn main() {
///     let path = ErasedActorPath::new("example.pinger").expect("a valid path");
///     let bytes = wire::encode_to_vec(&path).expect("encodes");
///
///     let refused = Carries::decode_with(&bytes, &mut DecodeCtx::empty()).map(|_| ());
///     assert_eq!(refused, Err(Error::ProtocolPathUnchecked { path }));
/// }
/// ```
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
        ProtocolPath::from_erased(self.as_erased().clone())
    }
}

/// A canonical path whose `Live` or `Dropped` route covers `P`. The canonical check comes
/// first, so a malformed or short path is `WireError::InvalidActorPath` and
/// the decoder is never asked; any actor covering `P` may live at the text,
/// so no leaf namespace is compared. The plain [`WireDecode::decode`] goes
/// through the `&[u8]` decoder, which refuses the route proof.
impl<'de, P: Protocol> WireDecode<'de> for ProtocolPath<P> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::decode_from(cursor)
    }

    fn decode_from<D: Decoder<'de> + ?Sized>(dec: &mut D) -> Result<Self, WireError> {
        let path = super::decode_canonical(dec.cursor())?;
        dec.prove_route_covers(&path, <P::Rows as RowSet>::CONTRACTS)?;
        Ok(Self::from_erased(path))
    }
}

typed_path_traits!(ProtocolPath<P>);
