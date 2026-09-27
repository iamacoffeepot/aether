//! Typed actor paths (ADR-0230 §2, ADR-0231 §3): [`ActorPath`] and
//! [`ProtocolPath`], descriptions with a codec, and [`ResolveError`], why a
//! receiver's `resolve` could not prove one.
//!
//! A typed path is an [`ErasedActorPath`] under a compile-time claim: that an
//! `R` lives at the text, or an actor covering the protocol `P`. The claim is
//! written by type constructors whose bounds check the topology
//! (`ActorPath::<R>::instance`, `ActorPath::<C>::child`), and narrowed only
//! where the compiler proves coverage. It crosses the wire as the path text
//! alone; a decoded path is canonical, a decoded `ActorPath<R>`'s leaf names
//! an `R`, and a decoded `ProtocolPath<P>` is checked at decode against the
//! engine's published route contracts: the live route at its path publishes
//! every row of `P`. On receipt, `resolve` proves liveness: that a live actor
//! still stands at the path. Neither type holds a position.
//!
//! The paths sit beside [`reference`](crate::reference), which holds the
//! proofs: a path names, a reference sends.

use alloc::vec::Vec;
use core::fmt::{self, Debug, Display, Formatter};
use core::hash::{Hash, Hasher};

use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{ActorPathForm, CastEligible, ErasedActorPath, LabelNode, Schema, SchemaType};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The value, kind-field, encode, and serialize traits both typed paths
/// share, over the text alone and with no bound on the phantom parameter.
/// `Clone`, `PartialEq`, `Eq`, and `Hash` compare the text. `Debug` prints
/// `Name("text")`, because a path is a name, not a position, and `Display`
/// prints the text. The kind-field, encode, and serialize traits delegate to
/// [`ErasedActorPath`]'s. Each type writes its own decodes beside it, over
/// [`decode_canonical`] (and, for `ActorPath`, [`deserialize_canonical`]).
/// Invoked beside each
/// type, whose module is a child of this one, so the trait names resolve
/// through `super::`.
macro_rules! typed_path_traits {
    ($name:ident<$param:ident>) => {
        impl<$param> Clone for $name<$param> {
            fn clone(&self) -> Self {
                Self::from_erased(self.path.clone())
            }
        }

        impl<$param> PartialEq for $name<$param> {
            fn eq(&self, other: &Self) -> bool {
                self.path == other.path
            }
        }

        impl<$param> Eq for $name<$param> {}

        impl<$param> super::Hash for $name<$param> {
            fn hash<H: super::Hasher>(&self, state: &mut H) {
                super::Hash::hash(&self.path, state);
            }
        }

        impl<$param> super::Debug for $name<$param> {
            fn fmt(&self, f: &mut super::Formatter<'_>) -> super::fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.path.as_str()).finish()
            }
        }

        impl<$param> super::Display for $name<$param> {
            fn fmt(&self, f: &mut super::Formatter<'_>) -> super::fmt::Result {
                super::Display::fmt(&self.path, f)
            }
        }

        impl<$param> super::Schema for $name<$param> {
            const SCHEMA: super::SchemaType = <super::ErasedActorPath as super::Schema>::SCHEMA;
            const LABEL: Option<&'static str> = <super::ErasedActorPath as super::Schema>::LABEL;
            const LABEL_NODE: super::LabelNode = <super::ErasedActorPath as super::Schema>::LABEL_NODE;
        }

        impl<$param> super::CastEligible for $name<$param> {
            const ELIGIBLE: bool = false;
        }

        impl<$param> super::WireEncode for $name<$param> {
            fn encode(&self, out: &mut super::Vec<u8>) -> Result<(), super::WireError> {
                super::WireEncode::encode(&self.path, out)
            }
        }

        impl<$param> super::Serialize for $name<$param> {
            fn serialize<S: super::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                super::Serialize::serialize(&self.path, serializer)
            }
        }
    };
}

mod actor_path;
mod protocol_path;
mod resolve_error;

pub use actor_path::ActorPath;
pub use protocol_path::ProtocolPath;
pub use resolve_error::ResolveError;

/// A decoded typed path is a well-formed canonical path (ADR-0230 §2,
/// ADR-0231 §3): a typed path is written from actor types, so it never has a
/// short path's holes, and `resolve` never expands one.
fn is_canonical(path: &ErasedActorPath) -> bool {
    matches!(path.form(), ActorPathForm::Canonical(_))
}

/// Wire-decode the text of a typed path: [`ErasedActorPath`]'s grammar, then
/// the canonical check. A short path is `WireError::InvalidActorPath`, the
/// refusal a malformed one gets.
fn decode_canonical(cursor: &mut &[u8]) -> Result<ErasedActorPath, WireError> {
    let path = ErasedActorPath::decode(cursor)?;
    if is_canonical(&path) {
        Ok(path)
    } else {
        Err(WireError::InvalidActorPath)
    }
}

/// Deserialize the text of a typed path: [`ErasedActorPath`]'s grammar, then
/// the canonical check, refusing a short path with a custom error that names
/// the rule.
fn deserialize_canonical<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ErasedActorPath, D::Error> {
    let path = ErasedActorPath::deserialize(deserializer)?;
    if is_canonical(&path) {
        Ok(path)
    } else {
        Err(D::Error::custom(ShortTypedPath(&path)))
    }
}

/// The serde refusal of a short typed path.
struct ShortTypedPath<'a>(&'a ErasedActorPath);

impl Display for ShortTypedPath<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "invalid typed actor path `{}`: a typed path is canonical and has no `:name` holes", self.0)
    }
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{DecodeCtx, PublishedRoutes, decode_from_slice};
    use aether_data::{Kind, KindId, ReplyContract};
    use alloc::string::{String, ToString};
    use alloc::sync::Arc;
    use core::cell::RefCell;
    use serde::de::IntoDeserializer;
    use serde::de::value::{Error as ValueError, StrDeserializer};

    use super::*;
    use crate::{Addressable, Many, Protocol, Row, RowSet, Silent};

    const CANONICAL: &str = "test.unit:alpha/test.member:beta";
    const SHORT: &str = "test.unit/:beta";

    struct Unit;

    impl Addressable for Unit {
        const NAMESPACE: &'static str = "test.unit";
        type Resolver = Many;
    }

    struct Member;

    impl Addressable for Member {
        const NAMESPACE: &'static str = "test.member";
        type Resolver = Many;
    }

    #[aether_data::kind(name = "test.path.poke")]
    struct Poke {
        seq: u32,
    }

    struct Poking;

    impl Protocol for Poking {
        type Rows = (Row<Poke, Silent>,);
    }

    #[aether_data::kind(name = "test.path.carries", no_serde)]
    struct Carries {
        path: ProtocolPath<Poking>,
    }

    /// Answers `rows` for every path, and records each path it was asked for.
    struct Recording {
        rows: Vec<(KindId, ReplyContract)>,
        asked: RefCell<Vec<ErasedActorPath>>,
    }

    impl Recording {
        fn answering(rows: &[(KindId, ReplyContract)]) -> Self {
            Self { rows: rows.to_vec(), asked: RefCell::new(Vec::new()) }
        }
    }

    impl PublishedRoutes for Recording {
        fn published_rows(&self, path: &ErasedActorPath) -> Option<Arc<[(KindId, ReplyContract)]>> {
            self.asked.borrow_mut().push(path.clone());
            Some(self.rows.as_slice().into())
        }
    }

    fn wire(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        ErasedActorPath::new(text).expect("a well-formed path").encode(&mut out).expect("encodes");
        out
    }

    fn serde_text(text: &str) -> StrDeserializer<'_, ValueError> {
        text.into_deserializer()
    }

    fn carried(text: &str, routes: &Recording) -> Result<String, WireError> {
        Carries::decode_with(&wire(text), &mut DecodeCtx::empty().routes(routes))
            .map(|carries| carries.path.to_string())
    }

    /// A typed path never expands, so a decode that let a hole through would
    /// hand `resolve` a path it could never fold. Both codecs of `ActorPath`
    /// run the canonical check and keep a canonical path's text.
    ///
    /// A `ProtocolPath` leaf that passed the wrong path or rows (an empty row
    /// set, which every route covers), or asked the context before the
    /// canonical check, would mint a path whose claim nothing proved: the leaf
    /// asks only for a canonical path, with exactly that text and `P`'s rows,
    /// and the plain shorthand, which has no context, refuses.
    #[test]
    fn decode_refuses_a_short_path_and_keeps_a_canonical_one() {
        assert_eq!(decode_from_slice::<ActorPath<Member>>(&wire(SHORT)), Err(WireError::InvalidActorPath));
        assert!(ActorPath::<Member>::deserialize(serde_text(SHORT)).is_err());

        let decoded = decode_from_slice::<ActorPath<Member>>(&wire(CANONICAL)).expect("a canonical path decodes");
        assert_eq!(decoded.to_string(), CANONICAL);
        let deserialized =
            ActorPath::<Member>::deserialize(serde_text(CANONICAL)).expect("a canonical path deserializes");
        assert_eq!(deserialized.to_string(), CANONICAL);

        let covering = Recording::answering(<<Poking as Protocol>::Rows as RowSet>::CONTRACTS);

        assert_eq!(carried(SHORT, &covering), Err(WireError::InvalidActorPath));
        assert!(covering.asked.borrow().is_empty(), "a short path is refused before the context is asked");

        assert_eq!(carried(CANONICAL, &covering).as_deref(), Ok(CANONICAL));
        assert_eq!(*covering.asked.borrow(), [ErasedActorPath::new(CANONICAL).expect("a well-formed path")]);

        let lacking = Recording::answering(&[(KindId(1), ReplyContract::None)]);

        assert_eq!(
            carried(CANONICAL, &lacking),
            Err(WireError::UncoveredProtocolPath {
                path: ErasedActorPath::new(CANONICAL).expect("a well-formed path"),
                kind: Poke::ID,
            }),
        );
        assert!(Carries::decode_from_bytes(&wire(CANONICAL)).is_none(), "the shorthand has no context");
    }
    /// An `ActorPath<R>` that exists names an `R`: a decode that claimed `R`
    /// for any text, read the root step instead of the leaf, or compared the
    /// leaf by prefix would hand a receiver a path naming another actor.
    /// Both codecs compare the leaf namespace with `R::NAMESPACE` exactly.
    #[test]
    fn decode_refuses_a_path_whose_leaf_names_another_actor() {
        const EXTENDED: &str = "test.unit:alpha/test.member.extra:beta";
        const SINGLETON: &str = "test.unit";

        assert!(decode_from_slice::<ActorPath<Member>>(&wire(CANONICAL)).is_ok());
        assert!(ActorPath::<Member>::deserialize(serde_text(CANONICAL)).is_ok());
        assert_eq!(decode_from_slice::<ActorPath<Unit>>(&wire(CANONICAL)), Err(WireError::InvalidActorPath));
        assert!(ActorPath::<Unit>::deserialize(serde_text(CANONICAL)).is_err());

        assert!(decode_from_slice::<ActorPath<Unit>>(&wire(SINGLETON)).is_ok());
        assert!(ActorPath::<Unit>::deserialize(serde_text(SINGLETON)).is_ok());

        assert_eq!(decode_from_slice::<ActorPath<Member>>(&wire(EXTENDED)), Err(WireError::InvalidActorPath));
        assert!(ActorPath::<Member>::deserialize(serde_text(EXTENDED)).is_err());
    }
}
