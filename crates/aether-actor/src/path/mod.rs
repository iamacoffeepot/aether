//! Typed actor paths (ADR-0230 §2, ADR-0231 §3): [`ActorPath`] and
//! [`ProtocolPath`], descriptions with a codec, and [`ResolveError`], why a
//! receiver's `resolve` could not prove one.
//!
//! A typed path is an [`ErasedActorPath`] under a compile-time claim: that an
//! `R` lives at the text, or an actor covering the protocol `P`. The claim is
//! written from actor types, by an actor that declares the link
//! (`#[actor(links(R))]`), and narrowed only where the compiler proves
//! coverage. It crosses the wire as the path text alone, so a decoded path
//! carries the writer's claim and nothing more, and it is proven again on
//! receipt by `resolve`. Neither type holds a position.
//!
//! The paths sit beside [`reference`](crate::reference), which holds the
//! proofs: a path names, a reference sends.

use alloc::vec::Vec;
use core::fmt::{self, Debug, Display, Formatter};
use core::hash::{Hash, Hasher};

use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{ActorPathForm, CastEligible, ErasedActorPath, LabelNode, LoadName, Schema, SchemaType};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Instanced, LinksTo, Root};

/// The value, kind-field, and serde traits both typed paths share, over the
/// text alone and with no bound on the phantom parameter. `Clone`,
/// `PartialEq`, `Eq`, and `Hash` compare the text. `Debug` prints
/// `Name("text")`, because a path is a name, not a position, and `Display`
/// prints the text. The kind-field and serde traits delegate to
/// [`ErasedActorPath`]'s, and each decode runs [`decode_canonical`] or
/// [`deserialize_canonical`]. Invoked beside each type, whose module is a
/// child of this one, so the trait names resolve through `super::`.
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

        impl<'de, $param> super::WireDecode<'de> for $name<$param> {
            fn decode(cursor: &mut &'de [u8]) -> Result<Self, super::WireError> {
                super::decode_canonical(cursor).map(Self::from_erased)
            }
        }

        impl<$param> super::Serialize for $name<$param> {
            fn serialize<S: super::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                super::Serialize::serialize(&self.path, serializer)
            }
        }

        impl<'de, $param> super::Deserialize<'de> for $name<$param> {
            fn deserialize<D: super::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                super::deserialize_canonical(deserializer).map(Self::from_erased)
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

/// Write `R`'s canonical path, `R::NAMESPACE:key`, for the native
/// `NativeCtx::link` in `aether-substrate`, which cannot reach the constructor
/// private to this crate.
///
/// It takes a key, never text, and writes only `R`'s own path, and it
/// compiles only for `A: LinksTo<R>`, the bound the calling verb already
/// carries. A call outside a linking actor's ctx writes a correct path that
/// proves and sends nothing. Not part of the public API.
#[doc(hidden)]
#[must_use]
pub fn __link<A: LinksTo<R>, R: Root + Instanced>(key: &LoadName) -> ActorPath<R> {
    ActorPath::root_instance(key)
}

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
    use aether_data::wire::decode_from_slice;
    use alloc::string::ToString;
    use serde::de::IntoDeserializer;
    use serde::de::value::{Error as ValueError, StrDeserializer};

    use super::*;

    const CANONICAL: &str = "test.unit:alpha/test.member:beta";
    const SHORT: &str = "test.unit/:beta";

    fn wire(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        ErasedActorPath::new(text).expect("a well-formed path").encode(&mut out).expect("encodes");
        out
    }

    fn serde_text(text: &str) -> StrDeserializer<'_, ValueError> {
        text.into_deserializer()
    }

    /// A typed path never expands, so a decode that let a hole through would
    /// hand `resolve` a path it could never fold. Both codecs of both types
    /// run the canonical check and keep a canonical path's text.
    #[test]
    fn decode_refuses_a_short_path_and_keeps_a_canonical_one() {
        assert_eq!(decode_from_slice::<ActorPath<()>>(&wire(SHORT)), Err(WireError::InvalidActorPath));
        assert_eq!(decode_from_slice::<ProtocolPath<()>>(&wire(SHORT)), Err(WireError::InvalidActorPath));
        assert!(ActorPath::<()>::deserialize(serde_text(SHORT)).is_err());
        assert!(ProtocolPath::<()>::deserialize(serde_text(SHORT)).is_err());

        let decoded = decode_from_slice::<ActorPath<()>>(&wire(CANONICAL)).expect("a canonical path decodes");
        assert_eq!(decoded.to_string(), CANONICAL);
        let decoded = decode_from_slice::<ProtocolPath<()>>(&wire(CANONICAL)).expect("a canonical path decodes");
        assert_eq!(decoded.to_string(), CANONICAL);
        let deserialized = ActorPath::<()>::deserialize(serde_text(CANONICAL)).expect("a canonical path deserializes");
        assert_eq!(deserialized.to_string(), CANONICAL);
        let deserialized =
            ProtocolPath::<()>::deserialize(serde_text(CANONICAL)).expect("a canonical path deserializes");
        assert_eq!(deserialized.to_string(), CANONICAL);
    }
}
