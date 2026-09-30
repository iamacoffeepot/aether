//! Staging a task's outputs through its source: bounded batches, one in
//! flight.
//!
//! A blob's chunks grow one buffer, hashed as they arrive, so
//! [`BlobWriter::finish`] names the blob with no round trip; the buffer is
//! then checked into the engine blob store on the worker and queued as an
//! [`EncodedArtifact::opaque_blob`]. A tree is queued as its encoding. A
//! batch goes out as one `Stage` once it holds [`STAGE_MAX_BYTES`] or
//! [`STAGE_MAX_ARTIFACTS`], and a blob larger than the byte bound goes alone.
//!
//! The next batch keeps filling while one `Stage` is in flight, and is sent
//! only once the one before it is answered `Staged`, so each is answered
//! before the next is sent (ADR-0240 D7). The codec stores children before
//! parents, so every citation in a `Stage` names an artifact in the same or
//! an earlier, already committed batch. Memory is bounded by two batches plus
//! the largest single blob.

use std::mem;

use aether_bloomery_kinds::{ArtifactHasher, EncodedArtifact, OpaqueBytes, Ref, Stage, StageResult, Tree};
use aether_bloomery_tar::{BlobWriter, TreeSink};
use aether_data::Kind;
use aether_substrate::actor::native::BlobCheckIn;

use super::{StorageAnswer, StorageCall, StorageError, StoragePort};

/// The payload bytes one `Stage` gathers before it is sent.
pub const STAGE_MAX_BYTES: u64 = 64 << 20;

/// The artifacts one `Stage` gathers before it is sent.
pub const STAGE_MAX_ARTIFACTS: usize = 4_096;

/// A session's staging: the batch filling and the `Stage` in flight.
#[derive(Default)]
pub struct Staging {
    filling: Vec<EncodedArtifact>,
    filling_bytes: u64,
    /// The sequence the `Stage` in flight will be answered under.
    in_flight: Option<u64>,
}

/// Stages through one session's port.
pub struct StagingSink<'a> {
    port: &'a mut StoragePort,
    staging: &'a mut Staging,
    check_in: &'a BlobCheckIn,
}

impl<'a> StagingSink<'a> {
    pub(super) fn new(port: &'a mut StoragePort, staging: &'a mut Staging, check_in: &'a BlobCheckIn) -> Self {
        Self { port, staging, check_in }
    }

    /// Send what is still filling, then wait for the last `Stage`'s answer.
    ///
    /// # Errors
    ///
    /// The first stage the source refused, or [`StorageError::Closed`].
    pub fn finish(&mut self) -> Result<(), StorageError> {
        self.flush()?;
        self.settle()
    }

    /// Queue `artifact`, of `len` payload bytes, sending the batch before it
    /// when `artifact` would carry it past the byte bound, and the batch it
    /// joins once that batch is full.
    fn queue(&mut self, artifact: EncodedArtifact, len: u64) -> Result<(), StorageError> {
        if !self.staging.filling.is_empty() && self.staging.filling_bytes.saturating_add(len) > STAGE_MAX_BYTES {
            self.flush()?;
        }
        self.staging.filling.push(artifact);
        self.staging.filling_bytes = self.staging.filling_bytes.saturating_add(len);
        if self.staging.filling_bytes >= STAGE_MAX_BYTES || self.staging.filling.len() >= STAGE_MAX_ARTIFACTS {
            self.flush()?;
        }
        Ok(())
    }

    /// Wait for the `Stage` in flight, then send the filled batch as the next.
    fn flush(&mut self) -> Result<(), StorageError> {
        if self.staging.filling.is_empty() {
            return Ok(());
        }
        self.settle()?;
        let batch = mem::take(&mut self.staging.filling);
        self.staging.filling_bytes = 0;
        self.staging.in_flight = Some(self.port.send(StorageCall::Stage(Stage::new(batch)))?);
        Ok(())
    }

    /// Wait for the answer to the `Stage` in flight, if one is.
    fn settle(&mut self) -> Result<(), StorageError> {
        let Some(seq) = self.staging.in_flight.take() else {
            return Ok(());
        };
        match self.port.wait(seq)? {
            StorageAnswer::Stage(StageResult::Staged) => Ok(()),
            StorageAnswer::Stage(StageResult::Err { message }) => Err(StorageError::Refused(message)),
            StorageAnswer::Read(_) | StorageAnswer::ReadMany(_) | StorageAnswer::ReadClosure(_) => {
                Err(StorageError::Answer)
            }
        }
    }
}

impl TreeSink for StagingSink<'_> {
    type Error = StorageError;
    type Blob<'b>
        = StagingBlob<'b>
    where
        Self: 'b;

    /// Start a blob. `len` is the archive's unverified claim, so nothing is
    /// allocated from it: the buffer grows as chunks arrive.
    fn begin_blob(&mut self, _len: u64) -> Result<StagingBlob<'_>, StorageError> {
        Ok(StagingBlob {
            sink: StagingSink { port: &mut *self.port, staging: &mut *self.staging, check_in: self.check_in },
            bytes: Vec::new(),
            hasher: ArtifactHasher::new(OpaqueBytes::ID),
        })
    }

    fn put_tree(&mut self, tree: &Tree) -> Result<Ref<Tree>, StorageError> {
        let artifact = EncodedArtifact::new(tree).map_err(|error| StorageError::Encode(error.to_string()))?;
        let staged = Ref::from_digest(artifact.digest());
        let len = artifact.len();
        self.queue(artifact, len)?;
        Ok(staged)
    }
}

/// One blob filling its buffer, staged when it is finished.
pub struct StagingBlob<'a> {
    sink: StagingSink<'a>,
    bytes: Vec<u8>,
    hasher: ArtifactHasher,
}

impl BlobWriter for StagingBlob<'_> {
    type Error = StorageError;

    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        self.hasher.update(bytes);
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(self) -> Result<Ref<OpaqueBytes>, StorageError> {
        let Self { mut sink, bytes, hasher } = self;
        let len = bytes.len() as u64;
        let payload = sink.check_in.check_in(bytes.into_boxed_slice());
        sink.queue(EncodedArtifact::opaque_blob(payload), len)?;
        Ok(Ref::from_digest(hasher.finish()))
    }
}
