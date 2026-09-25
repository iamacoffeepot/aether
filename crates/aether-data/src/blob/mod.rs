//! Blobs: immutable bytes as a value (ADR-0238 decisions 2, 4 and 9).
//!
//! A [`Blob`] means its bytes wherever it goes. Only its backing differs:
//! `Owned` bytes in hand, built by anyone with `Blob::from`, or a `Shared`
//! engine entry reached through [`BlobBacking`]. The engine alone builds a
//! `Shared` value, through the hidden `__mint_shared_blob`, which
//! `scripts/check-reference-mint.py` confines. A `Blob` has no whole-bytes
//! accessor: every read streams through [`BlobReader`].
//!
//! [`BlobHash`] is a blob's identity and dedup key. Knowing one grants
//! nothing, and nothing looks a blob up by it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

mod reader;
#[cfg(test)]
mod tests;

pub use reader::{BlobReader, MAX_READ_BYTES};

/// Immutable bytes. `Clone` is cheap: it clones one `Arc`, and the bytes stay
/// resident until the last clone drops.
#[derive(Clone)]
pub struct Blob(Repr);

#[derive(Clone)]
enum Repr {
    Owned(Arc<[u8]>),
    Shared(Arc<dyn BlobBacking>),
}

impl Blob {
    fn len(&self) -> u64 {
        match &self.0 {
            Repr::Owned(bytes) => bytes.len() as u64,
            Repr::Shared(backing) => backing.len(),
        }
    }
}

impl From<Vec<u8>> for Blob {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Repr::Owned(Arc::from(bytes)))
    }
}

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backing = match self.0 {
            Repr::Owned(_) => "owned",
            Repr::Shared(_) => "shared",
        };
        f.debug_struct("Blob").field("backing", &backing).field("len", &self.len()).finish()
    }
}

/// The bytes behind a `Shared` [`Blob`]. `aether-data` declares it; the
/// engine blob store and the guest backing implement it. Implementing it
/// builds nothing: only `__mint_shared_blob` turns a backing into a `Blob`.
pub trait BlobBacking: Send + Sync + 'static {
    /// The number of bytes.
    fn len(&self) -> u64;

    /// Copy bytes at `offset` into `buf` and return how many: at most
    /// `buf.len()`, possibly fewer; `0` only at or past the end, so a caller
    /// loops until it sees `0`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize;

    /// Whether there are no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The BLAKE3 digest of a blob's bytes: the blob's identity and the store's
/// dedup key. Knowing one grants nothing (ADR-0238 decision 4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlobHash([u8; 32]);

impl BlobHash {
    /// Wrap a 32-byte BLAKE3 digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// The engine's gated constructor of a `Shared` value: the store's check-in
/// and the guest backing. `scripts/check-reference-mint.py` confines it.
#[doc(hidden)]
#[must_use]
pub fn __mint_shared_blob(backing: Arc<dyn BlobBacking>) -> Blob {
    Blob(Repr::Shared(backing))
}
