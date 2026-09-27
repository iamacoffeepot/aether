//! The tar codec's [`TreeSink`] and [`TreeSource`] over one of the journal's
//! artifact batches.
//!
//! Decoding, each blob streams into a [`BlobFile`] a chunk at a time, so
//! nothing grows from the archive's claimed length, and each directory is
//! staged as an encoded [`Tree`]. The journal writes every file (ADR-0237 open
//! question 1); nothing here touches the filesystem, and nothing is visible
//! until the caller commits the batch.
//!
//! Encoding, each tree loads from its committed row and each blob streams from
//! its file through a [`VerifiedBlob`], which fails the read at end of stream
//! when the bytes do not hash to the digest, so a corrupt blob never crosses
//! into a container as whole.

#[cfg(test)]
mod tests;

use std::error::Error;
use std::fmt;

use aether_bloomery_journal::{ArtifactBatch, BlobFile, GetError, JournalError, VerifiedBlob};
use aether_bloomery_kinds::{Digest, OpaqueBytes, Ref, Tree};
use aether_bloomery_tar::{BlobWriter, SourceBlob, TreeSink, TreeSource};

/// Stores what a decode builds into `batch`.
pub struct JournalSink<'batch> {
    batch: &'batch mut ArtifactBatch,
}

impl<'batch> JournalSink<'batch> {
    pub fn new(batch: &'batch mut ArtifactBatch) -> Self {
        Self { batch }
    }
}

impl TreeSink for JournalSink<'_> {
    type Error = JournalError;
    type Blob<'a>
        = JournalBlob<'a>
    where
        Self: 'a;

    fn begin_blob(&mut self, len: u64) -> Result<JournalBlob<'_>, JournalError> {
        self.batch.blob(len).map(JournalBlob)
    }

    fn put_tree(&mut self, tree: &Tree) -> Result<Ref<Tree>, JournalError> {
        self.batch.stage_encoded::<Tree>(tree)
    }
}

/// One blob streaming into the batch.
pub struct JournalBlob<'a>(BlobFile<'a>);

impl BlobWriter for JournalBlob<'_> {
    type Error = JournalError;

    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), JournalError> {
        self.0.write_chunk(bytes)
    }

    fn finish(self) -> Result<Ref<OpaqueBytes>, JournalError> {
        self.0.finish()
    }
}

/// Loads what an encode walks from `batch`'s committed rows.
pub struct JournalSource<'batch> {
    batch: &'batch ArtifactBatch,
}

impl<'batch> JournalSource<'batch> {
    pub fn new(batch: &'batch ArtifactBatch) -> Self {
        Self { batch }
    }
}

impl TreeSource for JournalSource<'_> {
    type Error = SourceError;
    type Blob<'a>
        = VerifiedBlob
    where
        Self: 'a;

    fn tree(&mut self, tree: &Ref<Tree>) -> Result<Tree, SourceError> {
        self.batch.get::<Tree>(&tree.digest()).map_err(SourceError::Get)?.ok_or(SourceError::Missing(tree.digest()))
    }

    fn blob(&mut self, blob: &Ref<OpaqueBytes>) -> Result<SourceBlob<VerifiedBlob>, SourceError> {
        let reader = self.batch.blob_reader(blob).map_err(SourceError::Journal)?;
        let reader = reader.ok_or(SourceError::Missing(blob.digest()))?;
        Ok(SourceBlob { len: reader.payload_len(), reader })
    }
}

/// Why [`JournalSource`] could not load a tree or open a blob.
#[derive(Debug)]
pub enum SourceError {
    /// No committed row stores the digest.
    Missing(Digest),
    /// A tree's row did not load or decode.
    Get(GetError),
    /// A blob's row or file did not open.
    Journal(JournalError),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(digest) => write!(f, "the journal stores no artifact {digest}"),
            Self::Get(error) => write!(f, "loading a tree: {error}"),
            Self::Journal(error) => write!(f, "opening a blob: {error}"),
        }
    }
}

impl Error for SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Missing(_) => None,
            Self::Get(error) => Some(error),
            Self::Journal(error) => Some(error),
        }
    }
}
