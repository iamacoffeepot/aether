//! Blob framing of an artifact: the kind's eight-byte prefix, then the
//! payload, hashed whole into its [`Digest`].

use alloc::vec::Vec;

use sha2::{Digest as _, Sha256};

use super::Digest;
use crate::KindId;

/// Eight-byte little-endian [`KindId`] prefix. Same layout [`KindId`]'s wire encoding writes.
#[must_use]
pub fn artifact_prefix(kind: KindId) -> [u8; 8] {
    kind.0.to_le_bytes()
}

/// Prefix plus payload. The store hashes this whole blob.
#[must_use]
pub fn artifact_blob(kind: KindId, payload: &[u8]) -> Vec<u8> {
    let mut blob = artifact_prefix(kind).to_vec();
    blob.extend_from_slice(payload);
    blob
}

/// Digest of [`artifact_blob`].
#[must_use]
pub fn artifact_digest(kind: KindId, payload: &[u8]) -> Digest {
    let mut hasher = ArtifactHasher::new(kind);
    hasher.update(payload);
    hasher.finish()
}

/// The artifact digest computed a chunk at a time: sha256 over the kind's
/// eight-byte prefix, then every payload chunk in order.
///
/// Feeding a payload in any split gives the digest [`artifact_digest`] gives
/// for the whole payload, so a streamed blob and a staged one of the same
/// bytes have one name.
pub struct ArtifactHasher {
    sha: Sha256,
}

impl ArtifactHasher {
    /// A hasher that has consumed `kind`'s prefix and no payload.
    #[must_use]
    pub fn new(kind: KindId) -> Self {
        let mut sha = Sha256::new();
        sha.update(artifact_prefix(kind));
        Self { sha }
    }

    /// Consume the next payload chunk.
    pub fn update(&mut self, chunk: &[u8]) {
        self.sha.update(chunk);
    }

    /// The digest of the prefix and every chunk consumed.
    #[must_use]
    pub fn finish(self) -> Digest {
        Digest::from_bytes(self.sha.finalize().into())
    }
}

/// Sha256 of a stored blob, prefix included.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Digest::from_bytes(hasher.finalize().into())
}
