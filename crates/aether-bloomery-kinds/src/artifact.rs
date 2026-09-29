//! Artifact kinds and blob framing above a raw content-addressed store.
//!
//! The store stays raw: a stored blob is a row naming `sha256(bytes)` plus a
//! file of exactly those bytes, named by that digest. An artifact is an
//! abstraction above that blob, whose bytes are an eight-byte [`KindId`]
//! prefix followed by a payload. The digest covers the kind, so a digest names
//! one kind and one payload.

use alloc::vec;
use alloc::vec::Vec;

use aether_data::{Blob, BlobReader, Kind, KindId, MAX_READ_BYTES};
use sha2::{Digest as _, Sha256};

use crate::Digest;

/// Arbitrary bytes, no promise. Never instantiated; names a prefix.
pub struct OpaqueBytes;

impl Kind for OpaqueBytes {
    const NAME: &'static str = "bloomery.artifact.bytes";
    const ID: KindId = aether_data::storage_kind_id_from_name(Self::NAME);
}

/// UTF-8 text, validated when staged. Never instantiated; names a prefix.
pub struct Utf8Text;

impl Kind for Utf8Text {
    const NAME: &'static str = "bloomery.artifact.text";
    const ID: KindId = aether_data::storage_kind_id_from_name(Self::NAME);
}

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

/// The artifact digest of `kind` and `bytes`, streamed once through
/// [`BlobReader`] and [`ArtifactHasher`] with one scratch window no larger
/// than [`MAX_READ_BYTES`].
pub fn blob_digest(kind: KindId, bytes: &Blob) -> Digest {
    let window = usize::try_from(BlobReader::open(bytes).len()).map_or(MAX_READ_BYTES, |len| len.min(MAX_READ_BYTES));
    let mut scratch = vec![0; window];
    stream(kind, bytes, Sink::Scratch(&mut scratch)).digest
}

/// Where each read window lands.
pub enum Sink<'b> {
    /// One scratch window, reused by every read: hashing only.
    Scratch(&'b mut [u8]),
    /// A buffer of the whole payload's length; each read fills its place.
    Whole(&'b mut [u8]),
}

/// What [`stream`] read: the digest of the kind and every byte read, and how
/// many payload bytes that was.
pub struct Streamed {
    pub digest: Digest,
    pub read: usize,
}

/// Stream `bytes` through [`ArtifactHasher`] seeded with `kind`, one
/// [`BlobReader::read_range`] window at a time, until a read returns nothing.
pub fn stream(kind: KindId, bytes: &Blob, mut sink: Sink<'_>) -> Streamed {
    let reader = BlobReader::open(bytes);
    let mut hasher = ArtifactHasher::new(kind);
    let mut read = 0;
    loop {
        let window = match &mut sink {
            Sink::Scratch(scratch) => &mut scratch[..],
            Sink::Whole(whole) => &mut whole[read..],
        };
        let copied = reader.read_range(read as u64, window);
        if copied == 0 {
            return Streamed { digest: hasher.finish(), read };
        }
        hasher.update(&window[..copied]);
        read += copied;
    }
}
