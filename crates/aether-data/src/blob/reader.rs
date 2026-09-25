//! Streaming a blob's bytes (ADR-0238 decisions 9 and 11).
//!
//! [`BlobReader`] is the one door to a [`Blob`]'s bytes. The caller supplies
//! the buffer and each call copies at most [`MAX_READ_BYTES`]. It does not
//! implement `std::io::Read`, whose provided `read_to_end` is the whole-load
//! shortcut; a caller that wants every byte loops, and the loop is visible.

use super::{Blob, Repr};

/// The most bytes one [`BlobReader::read`] or [`BlobReader::read_range`]
/// copies.
pub const MAX_READ_BYTES: usize = 1 << 20;

/// A cursor over one [`Blob`]'s bytes.
pub struct BlobReader<'a> {
    blob: &'a Blob,
    cursor: u64,
}

impl<'a> BlobReader<'a> {
    /// A reader at the start of `blob`.
    #[must_use]
    pub fn open(blob: &'a Blob) -> Self {
        Self { blob, cursor: 0 }
    }

    /// The blob's length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.blob.len()
    }

    /// Whether the blob has no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy bytes from the cursor into `buf` and advance the cursor past
    /// them. Copies at most `MAX_READ_BYTES` and returns how many; `0` at the
    /// end, or when `buf` is empty.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let copied = self.read_range(self.cursor, buf);
        self.cursor += copied as u64;
        copied
    }

    /// Move the cursor to `to`, clamped to the length.
    pub fn seek(&mut self, to: u64) {
        self.cursor = to.min(self.len());
    }

    /// Copy bytes at `offset` into `buf` without moving the cursor. Copies at
    /// most `MAX_READ_BYTES` and returns how many; `0` at or past the end, or
    /// when `buf` is empty.
    pub fn read_range(&self, offset: u64, buf: &mut [u8]) -> usize {
        let window = buf.len().min(MAX_READ_BYTES);
        let buf = &mut buf[..window];
        match &self.blob.0 {
            Repr::Owned(bytes) => copy_from_slice_at(bytes, offset, buf),
            Repr::Shared(backing) => backing.read_at(offset, buf),
        }
    }
}

/// Copy the bytes of `bytes` at `offset` into `buf`, at most `buf.len()`, and
/// return how many; `0` at or past the end.
fn copy_from_slice_at(bytes: &[u8], offset: u64, buf: &mut [u8]) -> usize {
    let Some(rest) = usize::try_from(offset).ok().and_then(|start| bytes.get(start..)) else {
        return 0;
    };
    let copied = rest.len().min(buf.len());
    buf[..copied].copy_from_slice(&rest[..copied]);
    copied
}
