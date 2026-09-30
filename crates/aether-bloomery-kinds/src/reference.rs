//! Citations of a stored artifact: [`Ref`] with its kind in the type, and
//! [`ErasedRef`] with its kind known only at runtime.

use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{
    Citations, Cites, DocNode, Kind, KindId, LabelNode, Schema, SchemaType, Storage, StorageData, StorageLeaves,
};

use crate::Digest;
use crate::artifact::{OpaqueBytes, Utf8Text, artifact_digest};

/// The only storable citation: 32 bytes, transparent leaf, kind in the type.
pub struct Ref<K> {
    digest: Digest,
    _kind: PhantomData<fn() -> K>,
}

impl<K> Ref<K> {
    /// The digest this citation names.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// Wrap a digest as a citation of `K`. Unchecked; the store verifies the prefix.
    #[must_use]
    pub fn from_digest(digest: Digest) -> Self {
        Self { digest, _kind: PhantomData }
    }
}

impl<K: Kind> Ref<K> {
    /// The same citation with its kind moved from the type to a value.
    #[must_use]
    pub fn erase(self) -> ErasedRef {
        ErasedRef { parts: ErasedParts { kind: K::ID, digest: self.digest } }
    }
}

impl Ref<OpaqueBytes> {
    /// Digest of `payload` stored as [`OpaqueBytes`]. Stages nothing.
    #[must_use]
    pub fn of_bytes(payload: &[u8]) -> Self {
        Self::from_digest(artifact_digest(OpaqueBytes::ID, payload))
    }
}

impl Ref<Utf8Text> {
    /// Digest of `text` stored as [`Utf8Text`]. Stages nothing.
    #[must_use]
    pub fn of_text(text: &str) -> Self {
        Self::from_digest(artifact_digest(Utf8Text::ID, text.as_bytes()))
    }
}

impl<K: Storage + Clone> Ref<K> {
    /// Digest of `value` stored as encoded `K`. Stages nothing.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when encoding fails.
    pub fn of_encoded(value: &K) -> Result<Self, StorageError> {
        let payload = K::encode_storage(&StorageData::from_value(value.clone()))?;
        Ok(Self::from_digest(artifact_digest(K::ID, &payload)))
    }
}

impl<K> Clone for Ref<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for Ref<K> {}

impl<K> PartialEq for Ref<K> {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
    }
}

impl<K> Eq for Ref<K> {}

impl<K> Hash for Ref<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.digest.hash(state);
    }
}

impl<K> fmt::Debug for Ref<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Ref").field(&self.digest).finish()
    }
}

impl<K: Kind + 'static> Schema for Ref<K> {
    const SCHEMA: SchemaType = <[u8; 32] as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::Ref"));
    const LABEL_NODE: LabelNode = <[u8; 32] as Schema>::LABEL_NODE;
}

impl<K> aether_data::CrossesActors for Ref<K> {}
impl<K> aether_data::CrossesWire for Ref<K> {}

impl<K: Kind + 'static> StorageLeaves for Ref<K> {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        <[u8; 32] as StorageLeaves>::contribute(self.digest.as_bytes(), carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self::from_digest(Digest::from_bytes(<[u8; 32] as StorageLeaves>::assemble(carry, depth, source)?)))
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        <[u8; 32] as StorageLeaves>::is_absent(carry, depth, source)
    }
}

impl<K: Kind + 'static> WireEncode for Ref<K> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.digest.as_bytes().encode(out)
    }
}

impl<'de, K: Kind + 'static> WireDecode<'de> for Ref<K> {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Ok(Self::from_digest(Digest::from_bytes(<[u8; 32] as WireDecode>::decode(cursor)?)))
    }
}

impl<K: Kind + 'static> StorageElement for Ref<K> {
    const TAGGED: bool = <[u8; 32] as StorageElement>::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.digest.as_bytes().contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Ok(Self::from_digest(Digest::from_bytes(<[u8; 32] as StorageElement>::assemble_element(depth, cursor)?)))
    }
}

impl<K: Kind> Cites for Ref<K> {
    fn cites(&self, sink: &mut Citations) {
        sink.push(K::ID, self.digest.as_bytes());
    }
}

/// A citation whose kind is a value, for a holder that links no Rust type of
/// the cited kind. It cites like [`Ref`]: [`Cites`] pushes its kind and
/// digest, so a closure walk injects the artifact and the store checks its
/// prefix. [`Self::cast`] recovers the typed [`Ref`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ErasedRef {
    parts: ErasedParts,
}

/// The stored fields of an [`ErasedRef`]. Its derive emits the codecs;
/// [`ErasedRef`] adds the one citation the derive cannot, since a
/// [`KindId`] and a bare [`Digest`] each cite nothing.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, aether_data::Storage)]
struct ErasedParts {
    /// The kind the cited artifact is stored under.
    kind: KindId,
    /// The digest of the cited artifact.
    digest: Digest,
}

impl ErasedRef {
    /// Cite the artifact `digest` names, stored under `kind`. Unchecked, like
    /// [`Ref::from_digest`]; the store verifies the prefix.
    #[must_use]
    pub const fn new(kind: KindId, digest: Digest) -> Self {
        Self { parts: ErasedParts { kind, digest } }
    }

    /// The kind the cited artifact is stored under.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.parts.kind
    }

    /// The digest this citation names.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.parts.digest
    }

    /// The typed citation, when the cited kind is `K`.
    #[must_use]
    pub fn cast<K: Kind>(self) -> Option<Ref<K>> {
        (self.parts.kind == K::ID).then(|| Ref::from_digest(self.parts.digest))
    }
}

impl Schema for ErasedRef {
    const SCHEMA: SchemaType = ErasedParts::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::ErasedRef"));
    const LABEL_NODE: LabelNode = ErasedParts::LABEL_NODE;
    const DOC_NODE: DocNode = ErasedParts::DOC_NODE;
}

impl aether_data::CrossesActors for ErasedRef {}
impl aether_data::CrossesWire for ErasedRef {}

impl StorageLeaves for ErasedRef {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.parts.contribute(carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self { parts: ErasedParts::assemble(carry, depth, source)? })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        ErasedParts::is_absent(carry, depth, source)
    }
}

impl WireEncode for ErasedRef {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.parts.encode(out)
    }
}

impl<'de> WireDecode<'de> for ErasedRef {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Ok(Self { parts: ErasedParts::decode(cursor)? })
    }
}

impl StorageElement for ErasedRef {
    const TAGGED: bool = ErasedParts::TAGGED;

    fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
        self.parts.contribute_element(depth, out)
    }

    fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
        Ok(Self { parts: ErasedParts::assemble_element(depth, cursor)? })
    }
}

impl Cites for ErasedRef {
    fn cites(&self, sink: &mut Citations) {
        sink.push(self.parts.kind, self.parts.digest.as_bytes());
    }
}
