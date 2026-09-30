//! Encoded artifacts carried over mail: a kind, its storage payload, and the citations walked from it.

use alloc::vec;
use alloc::vec::Vec;

use aether_data::{
    Blob, BlobReader, Citations, Cites, Kind, KindId, MAX_READ_BYTES, Storage, StorageData, StorageError,
};

use crate::artifact::blob_digest;
use crate::{Digest, OpaqueBytes, Utf8Text};

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
/// The payload is a [`Blob`], so carrying an artifact copies nothing: a
/// checked-in payload crosses in process by hash. Every read of it streams
/// through [`BlobReader`].
///
/// The typed constructor collects citations, but a decoded mail is untrusted:
/// the journal verifies every supplied citation, not that the supplied list
/// is complete for arbitrary encoded payloads.
#[derive(Clone, Debug, aether_data::Schema)]
pub struct EncodedArtifact {
    kind: KindId,
    bytes: Blob,
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
            bytes: Blob::from(K::encode_storage(&StorageData::from_value(value.clone()))?),
            citations: citations
                .into_vec()
                .into_iter()
                .map(|citation| ArtifactCitation { kind: citation.kind, bytes: citation.bytes })
                .collect(),
        })
    }

    /// Stage `payload` as [`OpaqueBytes`] with no citations.
    ///
    /// The digest matches `Batch::stage_bytes`: `artifact_blob(OpaqueBytes::ID, payload)`.
    #[must_use]
    pub fn opaque_bytes(payload: &[u8]) -> Self {
        Self::opaque_blob(Blob::from(payload.to_vec()))
    }

    /// Stage bytes already held as a [`Blob`] as [`OpaqueBytes`] with no
    /// citations, without copying them. A checked-in payload stays in the
    /// engine blob store and crosses by hash.
    ///
    /// The digest matches [`Self::opaque_bytes`] over the same bytes.
    #[must_use]
    pub fn opaque_blob(payload: Blob) -> Self {
        Self { kind: OpaqueBytes::ID, bytes: payload, citations: Vec::new() }
    }

    /// Stage UTF-8 `text` as [`Utf8Text`] with no citations.
    ///
    /// The digest matches `Batch::stage_text`: `artifact_blob(Utf8Text::ID, text.as_bytes())`.
    #[must_use]
    pub fn text(text: &str) -> Self {
        Self { kind: Utf8Text::ID, bytes: Blob::from(text.as_bytes().to_vec()), citations: Vec::new() }
    }

    /// Stage `payload`, already encoded, under `kind` with no citations: the
    /// form for a stager that links no Rust type of `kind`.
    ///
    /// The journal verifies every citation an artifact supplies, not that
    /// the list is complete, so a payload that cites artifacts is staged
    /// here as if it cited none; the stager is the one that says so.
    #[must_use]
    pub fn uncited(kind: KindId, payload: &[u8]) -> Self {
        Self { kind, bytes: Blob::from(payload.to_vec()), citations: Vec::new() }
    }

    /// Storage kind of the encoded value; the stored blob's prefix.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// The payload length in bytes, without the kind prefix. It reads no bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        BlobReader::open(&self.bytes).len()
    }

    /// Whether the payload is empty. It reads no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Citations walked from the typed value.
    #[must_use]
    pub fn citations(&self) -> &[ArtifactCitation] {
        &self.citations
    }

    /// Digest the journal assigns once the artifact is staged, streamed
    /// through [`BlobReader`] a window at a time.
    #[must_use]
    pub fn digest(&self) -> Digest {
        blob_digest(self.kind, &self.bytes)
    }

    /// Take the kind, payload, and citations without copying payload bytes.
    #[must_use]
    pub fn into_parts(self) -> (KindId, Blob, Vec<ArtifactCitation>) {
        (self.kind, self.bytes, self.citations)
    }
}

/// Equal kind, equal citations, and equal payload bytes, compared a
/// [`BlobReader`] window at a time. [`Blob`] has no equality of its own.
impl PartialEq for EncodedArtifact {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.citations == other.citations && same_bytes(&self.bytes, &other.bytes)
    }
}

impl Eq for EncodedArtifact {}

/// Whether `left` and `right` hold the same bytes, read in windows no larger
/// than [`MAX_READ_BYTES`].
fn same_bytes(left: &Blob, right: &Blob) -> bool {
    let (left, right) = (BlobReader::open(left), BlobReader::open(right));
    if left.len() != right.len() {
        return false;
    }
    let window = usize::try_from(left.len()).map_or(MAX_READ_BYTES, |len| len.min(MAX_READ_BYTES));
    let (mut left_window, mut right_window) = (vec![0; window], vec![0; window]);
    let mut offset = 0;
    loop {
        let offered = left.read_range(offset, &mut left_window);
        if offered == 0 {
            return offset == left.len();
        }
        let matched = right.read_range(offset, &mut right_window[..offered]);
        if matched == 0 || left_window[..matched] != right_window[..matched] {
            return false;
        }
        offset += matched as u64;
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use aether_data::{Blob, MAX_READ_BYTES};

    use super::EncodedArtifact;

    #[test]
    fn equality_reads_every_payload_window() {
        // Catches an equality that compares only lengths, only the kind's digest, or only the
        // first read window: the two payloads differ in one byte past it.
        let payload = vec![b'a'; MAX_READ_BYTES + 2];
        let mut altered = payload.clone();
        altered[MAX_READ_BYTES + 1] = b'b';

        assert_ne!(EncodedArtifact::opaque_bytes(&payload), EncodedArtifact::opaque_bytes(&altered));
        assert_eq!(EncodedArtifact::opaque_bytes(&payload), EncodedArtifact::opaque_blob(Blob::from(payload)));
    }
}
