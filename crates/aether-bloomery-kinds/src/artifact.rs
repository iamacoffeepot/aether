//! Streaming an artifact's digest out of a [`Blob`] a window at a time.
//!
//! The framing and the hasher live in `aether-data`
//! ([`ArtifactHasher`], [`aether_data::artifact_digest`]); these helpers feed
//! a [`Blob`] through them for the journal and the program invoker.

use alloc::vec;

use aether_data::{ArtifactHasher, Blob, BlobReader, Digest, KindId, MAX_READ_BYTES};

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
