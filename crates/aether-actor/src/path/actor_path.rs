//! [`ActorPath<R>`]: an actor-typed path (ADR-0230 §2).

use alloc::format;
use core::fmt::{self, Display, Formatter};
use core::marker::PhantomData;

use aether_data::wire::{Error as WireError, WireDecode};
use aether_data::{ActorPathError, ErasedActorPath, LoadName, Namespace};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer};

use crate::{Addressable, ChildOf, Instanced, Root};

/// The canonical path of an `R`: an [`ErasedActorPath`] whose leaf names an
/// `R` (ADR-0230 §2).
///
/// Written, the text is `R`'s canonical path, each step a type's `NAMESPACE`
/// and its key, by one of two constructors whose bounds are the topology
/// check: [`ActorPath::<R>::instance(&key)`](Self::instance) for a root
/// instance and [`ActorPath::<C>::child(&parent, &key)`](Self::child) for an
/// instanced child. Decoded, it is a well-formed canonical path whose leaf
/// namespace is `R::NAMESPACE`, so an `ActorPath<R>` that exists names an
/// `R`. Existence and liveness are `resolve`'s to prove, and it grants no
/// send.
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
    /// The typed view of `path`. The callers are the constructors below and
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
    /// # Panics
    ///
    /// Never: the inline `const` makes an invalid `NAMESPACE` a compile error
    /// where this is monomorphized, and one step of two valid segments is at
    /// most 513 bytes at depth 1, under both caps.
    #[must_use]
    pub fn instance(key: &LoadName) -> Self {
        let _ = const { Namespace::new(R::NAMESPACE) };
        let text = format!("{}:{}", R::NAMESPACE, key.as_str());

        Self::from_erased(ErasedActorPath::new(&text).expect("one namespace and key step is a valid path"))
    }
}

impl<C: Instanced> ActorPath<C> {
    /// `<parent>/C::NAMESPACE:key`, the name the registry gives the instanced
    /// `C` spawned under `key` beneath the actor at `parent`. Reads no
    /// registry and folds nothing.
    ///
    /// # Errors
    ///
    /// [`ErasedActorPath::new`]'s refusal, which here can only be the
    /// depth or byte cap: the parent is a valid canonical path, the inline
    /// `const` makes an invalid `C::NAMESPACE` a compile error, and the key
    /// is a valid segment.
    pub fn child<P: Addressable>(parent: &ActorPath<P>, key: &LoadName) -> Result<Self, ActorPathError>
    where
        C: ChildOf<P>,
    {
        let _ = const { Namespace::new(C::NAMESPACE) };
        let text = format!("{}/{}:{}", parent.path, C::NAMESPACE, key.as_str());

        ErasedActorPath::new(&text).map(Self::from_erased)
    }
}

/// The leaf namespace of a canonical path: its last `/` step, up to that
/// step's first `:`, or the whole step when it has no key.
fn leaf_namespace(path: &ErasedActorPath) -> &str {
    let text = path.as_str();
    let leaf = text.rsplit_once('/').map_or(text, |(_, leaf)| leaf);

    leaf.split_once(':').map_or(leaf, |(namespace, _)| namespace)
}

/// Whether `path`'s leaf names an `R`: exact equality with `R::NAMESPACE`,
/// never a prefix.
fn names<R: Addressable>(path: &ErasedActorPath) -> bool {
    leaf_namespace(path) == R::NAMESPACE
}

/// A canonical path whose leaf names an `R`, or `WireError::InvalidActorPath`,
/// the refusal a short or malformed path gets.
impl<'de, R: Addressable> WireDecode<'de> for ActorPath<R> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        let path = super::decode_canonical(cursor)?;
        if names::<R>(&path) {
            Ok(Self::from_erased(path))
        } else {
            Err(WireError::InvalidActorPath)
        }
    }
}

/// A canonical path whose leaf names an `R`, or a custom error naming the
/// path, its leaf, and `R::NAMESPACE`.
impl<'de, R: Addressable> Deserialize<'de> for ActorPath<R> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let path = super::deserialize_canonical(deserializer)?;
        if names::<R>(&path) {
            Ok(Self::from_erased(path))
        } else {
            Err(D::Error::custom(ForeignLeaf { path: &path, expected: R::NAMESPACE }))
        }
    }
}

/// The serde refusal of a typed path whose leaf names another actor.
struct ForeignLeaf<'a> {
    path: &'a ErasedActorPath,
    expected: &'static str,
}

impl Display for ForeignLeaf<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid typed actor path `{}`: its leaf `{}` is not `{}`",
            self.path,
            leaf_namespace(self.path),
            self.expected
        )
    }
}

typed_path_traits!(ActorPath<R>);
