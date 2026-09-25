//! Blob references: immutable checked-in bytes named by their hash
//! (ADR-0238 decisions 2, 4 and 9).
//!
//! [`BlobHash`] is a blob's identity and knowing one grants nothing. A native
//! [`BlobRef`] holds the hash and its store entry's `Arc` behind
//! [`BlobBacking`], so cloning adds a reference, dropping lets it go, and
//! [`BlobRef::bytes`] borrows without a ctx and cannot fail. The engine blob
//! store is the only source of a `BlobRef`: its constructor is the hidden
//! `__mint_blob_ref`, and `scripts/check-reference-mint.py` confines that to
//! the store.

#[cfg(not(target_arch = "wasm32"))]
use alloc::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use core::fmt;

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
}

/// Bytes that a native [`BlobRef`] keeps resident. `aether-data` declares it
/// and the engine blob store implements it. It is not a constructor: a
/// `BlobRef` comes only from the store.
#[cfg(not(target_arch = "wasm32"))]
pub trait BlobBacking: Send + Sync + 'static {
    /// The resident bytes.
    fn bytes(&self) -> &[u8];
}

/// A reference to immutable checked-in bytes, named by their hash (ADR-0238
/// decision 2). Not `Copy`: `Clone` adds a reference, and dropping the last
/// reference lets the bytes go. No release verb exists.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct BlobRef {
    hash: BlobHash,
    backing: Arc<dyn BlobBacking>,
}

#[cfg(not(target_arch = "wasm32"))]
impl BlobRef {
    /// Borrow the bytes zero-copy. A native `BlobRef` always holds its entry,
    /// so this cannot fail and needs no ctx.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.backing.bytes()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl fmt::Debug for BlobRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobRef").field("hash", &self.hash).finish_non_exhaustive()
    }
}

/// The gated mint. Its only caller is the engine blob store, and
/// `scripts/check-reference-mint.py` enforces that.
#[cfg(not(target_arch = "wasm32"))]
#[doc(hidden)]
#[must_use]
pub fn __mint_blob_ref(hash: BlobHash, backing: Arc<dyn BlobBacking>) -> BlobRef {
    BlobRef { hash, backing }
}
