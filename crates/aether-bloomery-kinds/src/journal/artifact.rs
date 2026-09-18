//! Encoded artifacts carried over mail: a kind, its storage payload, and the citations walked from it.

use alloc::vec::Vec;

use aether_data::{Citations, Cites, KindId, Storage, StorageData, StorageError};

use crate::{Digest, artifact_digest};

/// One portable citation collected from an encoded artifact.
#[derive(Clone, Debug, PartialEq, Eq, aether_data::Schema)]
pub struct ArtifactCitation {
    kind: KindId,
    bytes: Vec<u8>,
}

impl ArtifactCitation {
    /// Expected kind of the cited artifact.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// Identity bytes of the cited artifact. The journal checks their width.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Take the citation's kind and identity bytes without copying them.
    #[must_use]
    pub fn into_parts(self) -> (KindId, Vec<u8>) {
        (self.kind, self.bytes)
    }
}

/// One typed value encoded for staging: its kind, unprefixed storage payload, and citations.
///
/// The typed constructor collects citations, but a decoded mail is untrusted:
/// the journal verifies every supplied citation, not that the supplied list
/// is complete for arbitrary encoded payloads.
#[derive(Clone, Debug, PartialEq, Eq, aether_data::Schema)]
pub struct EncodedArtifact {
    kind: KindId,
    bytes: Vec<u8>,
    citations: Vec<ArtifactCitation>,
}

impl EncodedArtifact {
    /// Encode `value` under `K::ID` and walk it for citations.
    ///
    /// # Errors
    ///
    /// Returns a storage error if `value` cannot be encoded.
    pub fn new<K: Storage + Clone + Cites>(value: &K) -> Result<Self, StorageError> {
        let mut citations = Citations::default();
        value.cites(&mut citations);
        Ok(Self {
            kind: K::ID,
            bytes: K::encode_storage(&StorageData::from_value(value.clone()))?,
            citations: citations
                .into_vec()
                .into_iter()
                .map(|citation| ArtifactCitation { kind: citation.kind, bytes: citation.bytes })
                .collect(),
        })
    }

    /// Storage kind of the encoded value; the stored blob's prefix.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// Encoded storage payload without its kind prefix.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Citations walked from the typed value.
    #[must_use]
    pub fn citations(&self) -> &[ArtifactCitation] {
        &self.citations
    }

    /// Digest the journal assigns once the artifact is staged.
    #[must_use]
    pub fn digest(&self) -> Digest {
        artifact_digest(self.kind, &self.bytes)
    }

    /// Take the kind, payload, and citations without copying payload bytes.
    #[must_use]
    pub fn into_parts(self) -> (KindId, Vec<u8>, Vec<ArtifactCitation>) {
        (self.kind, self.bytes, self.citations)
    }
}
