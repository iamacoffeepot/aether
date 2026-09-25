//! Streaming a blob in and out: [`BlobFile`] writes one, [`VerifiedBlob`] reads one back.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::mem;

use aether_bloomery_kinds::{ArtifactHasher, Digest, OpaqueBytes, Ref, artifact_prefix};
use aether_data::Kind;
use tempfile::NamedTempFile;

use super::ArtifactBatch;
use crate::blobs::BlobDir;
use crate::journal::JournalError;

/// Length of the kind prefix every blob file starts with.
const PREFIX_BYTES: u64 = 8;

/// One [`OpaqueBytes`] blob being streamed into its batch.
///
/// Opened by [`ArtifactBatch::blob`] with the payload length it must reach.
/// Each chunk is written to a temp file in `blobs/tmp/` and hashed; memory
/// is bounded by the caller's chunk, never by the blob. Dropping it before
/// [`BlobFile::finish`] deletes the temp file and records nothing. A chunk
/// whose write fails may leave part of itself in the file, so the blob is
/// broken from then on: every later chunk and `finish` are refused.
pub struct BlobFile<'batch> {
    batch: &'batch mut ArtifactBatch,
    staged: NamedTempFile,
    hasher: ArtifactHasher,
    expected_bytes: u64,
    written_bytes: u64,
    broken: bool,
}

impl<'batch> BlobFile<'batch> {
    /// Create the temp file and write and hash the [`OpaqueBytes`] prefix.
    pub(super) fn open(batch: &'batch mut ArtifactBatch, expected_bytes: u64) -> Result<Self, JournalError> {
        let mut staged = batch.blobs.temp_file()?;
        staged.write_all(&artifact_prefix(OpaqueBytes::ID)).map_err(|error| JournalError::io(staged.path(), error))?;
        Ok(Self {
            batch,
            staged,
            hasher: ArtifactHasher::new(OpaqueBytes::ID),
            expected_bytes,
            written_bytes: 0,
            broken: false,
        })
    }

    /// Write and hash the next payload chunk.
    ///
    /// # Errors
    ///
    /// [`JournalError::BlobLength`] when the chunk would carry the payload
    /// past the length the blob was opened with; nothing of it is written.
    /// [`JournalError::Io`] when the write fails, or an earlier one did.
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), JournalError> {
        self.refuse_if_broken()?;
        let offered = u64::try_from(chunk.len())
            .ok()
            .and_then(|len| self.written_bytes.checked_add(len))
            .ok_or(JournalError::IntegerRange)?;
        if offered > self.expected_bytes {
            return Err(JournalError::BlobLength { expected_bytes: self.expected_bytes, actual_bytes: offered });
        }
        if let Err(error) = self.staged.write_all(chunk) {
            self.broken = true;
            return Err(JournalError::io(self.staged.path(), error));
        }
        self.hasher.update(chunk);
        self.written_bytes = offered;
        Ok(())
    }

    /// Make the blob durable under its digest name and record its row in the
    /// batch: fsync the temp file, rename it to the digest name, fsync the
    /// shard directory. When the digest name already exists the temp file is
    /// deleted instead and the shard directory is fsynced.
    ///
    /// # Errors
    ///
    /// [`JournalError::BlobLength`] when fewer payload bytes were written than
    /// the blob was opened with; the temp file is deleted. [`JournalError::Io`]
    /// when the sync or rename fails, or when a chunk's write failed.
    pub fn finish(self) -> Result<Ref<OpaqueBytes>, JournalError> {
        self.refuse_if_broken()?;
        let Self { batch, staged, hasher, expected_bytes, written_bytes, broken: _ } = self;
        if written_bytes != expected_bytes {
            return Err(JournalError::BlobLength { expected_bytes, actual_bytes: written_bytes });
        }
        let digest = hasher.finish();
        batch.blobs.place(&digest, staged)?;
        batch.record(digest, PREFIX_BYTES.checked_add(written_bytes).ok_or(JournalError::IntegerRange)?, Vec::new());
        Ok(Ref::from_digest(digest))
    }

    /// Refuse to go on once a chunk's write has failed: the file may hold
    /// bytes the hasher never saw.
    fn refuse_if_broken(&self) -> Result<(), JournalError> {
        if self.broken {
            Err(JournalError::io(self.staged.path(), io::Error::other("an earlier chunk of this blob failed to write")))
        } else {
            Ok(())
        }
    }
}

/// A committed [`OpaqueBytes`] blob's payload, read as a stream and verified
/// against its digest.
///
/// Reading hashes the prefix and every payload byte. At end of stream a
/// payload whose length is not the row's, or whose bytes do not hash to the
/// digest, is an [`ErrorKind::InvalidData`] error instead of `Ok(0)`, and
/// stays one on every later read, so a caller that reads to the end never
/// takes corrupt bytes as whole.
pub struct VerifiedBlob {
    file: File,
    verdict: Verdict,
    digest: Digest,
    payload_bytes: u64,
    read_bytes: u64,
}

/// Where a [`VerifiedBlob`] stands: still hashing, or judged at end of stream.
enum Verdict {
    Reading(ArtifactHasher),
    Whole,
    Corrupt(String),
}

impl VerifiedBlob {
    /// Open `digest`'s file, whose row records `size_bytes`, positioned past its prefix.
    pub(super) fn open(blobs: &BlobDir, digest: Digest, size_bytes: u64) -> Result<Self, JournalError> {
        let payload_bytes = size_bytes.checked_sub(PREFIX_BYTES).ok_or(JournalError::CorruptArtifact)?;
        let path = blobs.path_of(&digest);
        let mut file = File::open(&path).map_err(|error| match error.kind() {
            ErrorKind::NotFound => JournalError::MissingBlob(digest),
            _ => JournalError::io(&path, error),
        })?;
        file.seek(SeekFrom::Start(PREFIX_BYTES)).map_err(|error| JournalError::io(&path, error))?;
        Ok(Self {
            file,
            verdict: Verdict::Reading(ArtifactHasher::new(OpaqueBytes::ID)),
            digest,
            payload_bytes,
            read_bytes: 0,
        })
    }

    /// The payload length the blob's row records: the bytes a whole read yields.
    #[must_use]
    pub fn payload_len(&self) -> u64 {
        self.payload_bytes
    }

    /// Judge the stream once it ends, then answer every end-of-stream read
    /// with that verdict.
    fn end_of_stream(&mut self) -> io::Result<usize> {
        self.verdict = match mem::replace(&mut self.verdict, Verdict::Whole) {
            Verdict::Reading(hasher) => self.judge(hasher),
            judged => judged,
        };
        match &self.verdict {
            Verdict::Corrupt(message) => Err(io::Error::new(ErrorKind::InvalidData, message.clone())),
            Verdict::Reading(_) | Verdict::Whole => Ok(0),
        }
    }

    /// The payload must be the recorded length and hash to the digest.
    fn judge(&self, hasher: ArtifactHasher) -> Verdict {
        if self.read_bytes != self.payload_bytes {
            Verdict::Corrupt(format!(
                "blob {} payload is {} bytes, its row records {}",
                self.digest, self.read_bytes, self.payload_bytes
            ))
        } else if hasher.finish() != self.digest {
            Verdict::Corrupt(format!("blob {} bytes do not hash to its digest", self.digest))
        } else {
            Verdict::Whole
        }
    }
}

impl Read for VerifiedBlob {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let read = self.file.read(buf)?;
        if read == 0 {
            return self.end_of_stream();
        }
        if let Verdict::Reading(hasher) = &mut self.verdict {
            hasher.update(&buf[..read]);
        }
        self.read_bytes = self.read_bytes.saturating_add(u64::try_from(read).map_err(io::Error::other)?);
        Ok(read)
    }
}
