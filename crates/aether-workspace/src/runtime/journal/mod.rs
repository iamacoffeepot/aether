//! The tar codec's [`TreeSink`] over one of the journal's artifact batches.
//!
//! Each blob streams into a [`BlobFile`] a chunk at a time, so nothing grows
//! from the archive's claimed length, and each directory is staged as an
//! encoded [`Tree`]. The journal writes every file (ADR-0237 open question 1);
//! nothing here touches the filesystem, and nothing is visible until the
//! caller commits the batch.

#[cfg(test)]
mod tests;

use aether_bloomery_journal::{ArtifactBatch, BlobFile, JournalError};
use aether_bloomery_kinds::{OpaqueBytes, Ref, Tree};
use aether_bloomery_tar::{BlobWriter, TreeSink};

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
