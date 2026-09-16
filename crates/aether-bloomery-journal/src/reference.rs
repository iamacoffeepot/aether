//! Typed citation of a stored artifact.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Citations, Cites, Kind, LabelNode, Schema, SchemaType, Storage, StorageData, StorageLeaves};

use crate::artifact::{Digest, OpaqueBytes, Utf8Text, artifact_digest};
use crate::batch::BatchError;

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
    /// [`BatchError::Storage`] when encoding fails.
    pub fn of_encoded(value: &K) -> Result<Self, BatchError> {
        let payload = K::encode_storage(&StorageData::from_value(value.clone())).map_err(BatchError::Storage)?;
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
        sink.push(K::ID, *self.digest.as_bytes());
    }
}
