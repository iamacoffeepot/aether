//! Where the codec reads blobs and trees from, and writes them to.

use std::io::Read;

use aether_bloomery_kinds::{OpaqueBytes, Ref, Tree};

/// Loads what [`crate::encode()`] walks: trees by reference, and each blob as a
/// reader whose length is known before the first byte is read.
pub trait TreeSource {
    /// Why a lookup failed. Surfaces as [`crate::EncodeError::Source`].
    type Error;
    /// A reader over one blob's bytes.
    type Blob<'a>: Read
    where
        Self: 'a;

    /// Load the tree `tree` names.
    ///
    /// # Errors
    ///
    /// Whatever the store reports, such as a missing tree.
    fn tree(&mut self, tree: &Ref<Tree>) -> Result<Tree, Self::Error>;

    /// Open the blob `blob` names. The reader must yield exactly `len` bytes;
    /// the encoder checks, and a short or long reader is
    /// [`crate::EncodeError::BlobLength`].
    ///
    /// # Errors
    ///
    /// Whatever the store reports, such as a missing blob.
    fn blob(&mut self, blob: &Ref<OpaqueBytes>) -> Result<SourceBlob<Self::Blob<'_>>, Self::Error>;
}

/// One blob opened by a [`TreeSource`]: its length, stated before the header
/// that carries it is written, and a reader over its bytes.
pub struct SourceBlob<R> {
    /// The blob's length in bytes.
    pub len: u64,
    /// A reader that yields exactly `len` bytes.
    pub reader: R,
}

/// Stores what [`crate::decode()`] builds. Push-style: the codec copies exactly
/// an entry's bytes into a [`BlobWriter`] and then finishes it, so a sink
/// cannot under-read an entry. The sink computes every [`Ref`], because a
/// store hashes what it stores.
pub trait TreeSink {
    /// Why a write failed. Surfaces as [`crate::DecodeError::Sink`].
    type Error;
    /// A writer for one blob of a length known up front.
    type Blob<'a>: BlobWriter<Error = Self::Error>
    where
        Self: 'a;

    /// Start a blob of exactly `len` bytes.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    fn begin_blob(&mut self, len: u64) -> Result<Self::Blob<'_>, Self::Error>;

    /// Store one finished directory and return its reference. Called once per
    /// directory, children before parents.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    fn put_tree(&mut self, tree: &Tree) -> Result<Ref<Tree>, Self::Error>;
}

/// Receives one blob's bytes in order. The codec writes exactly the length
/// passed to [`TreeSink::begin_blob`], then calls [`BlobWriter::finish`].
pub trait BlobWriter {
    /// Why a write failed.
    type Error;

    /// Append the next chunk of the blob.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Seal the blob and return its reference.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    fn finish(self) -> Result<Ref<OpaqueBytes>, Self::Error>
    where
        Self: Sized;
}
