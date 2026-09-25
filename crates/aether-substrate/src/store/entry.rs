//! A resident entry and its dedup key.

use std::fmt;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use aether_data::{Blob, BlobBacking, BlobHash};

use super::{Index, Shared, reclaim};

/// The BLAKE3 digest of `bytes` as the entry's identity and dedup key.
pub(super) fn hash_of(bytes: &[u8]) -> BlobHash {
    BlobHash::from_bytes(*blake3::hash(bytes).as_bytes())
}

/// Immutable checked-in bytes, shared as an `Arc`. Reading them takes no lock.
/// The entry is freed when its last strong reference drops; see the module
/// docs for what that drop does.
pub struct BlobEntry {
    hash: BlobHash,
    bytes: Box<[u8]>,
    /// The store state the drop path needs: the index, the resident-byte
    /// counter and the reclaim sender.
    home: Arc<Shared>,
}

impl BlobEntry {
    pub(super) fn new(hash: BlobHash, bytes: Box<[u8]>, home: Arc<Shared>) -> Self {
        Self { hash, bytes, home }
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// The entry's hash: the key a tag-1 `Blob` field names it by, which the
    /// egress rewrite matches against an envelope's attachments.
    #[must_use]
    pub fn hash(&self) -> BlobHash {
        self.hash
    }

    /// Hold this entry as a `Shared` [`Blob`]: the store's one mint site. The
    /// value keeps the entry, and so its bytes, resident until it and every
    /// clone of it drop.
    pub(crate) fn into_blob(self: Arc<Self>) -> Blob {
        aether_data::__mint_shared_blob(self)
    }
}

impl fmt::Debug for BlobEntry {
    /// The identity and size, never the bytes: an entry can be very large.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobEntry").field("hash", &self.hash).field("len", &self.bytes.len()).finish_non_exhaustive()
    }
}

impl BlobBacking for BlobEntry {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize {
        let Some(rest) = usize::try_from(offset).ok().and_then(|start| self.bytes.get(start..)) else {
            return 0;
        };
        let copied = rest.len().min(buf.len());
        buf[..copied].copy_from_slice(&rest[..copied]);
        copied
    }
}

impl Drop for BlobEntry {
    fn drop(&mut self) {
        let bytes = mem::take(&mut self.bytes);
        self.home.resident_bytes.fetch_sub(bytes.len(), Ordering::Relaxed);

        let this: *const Self = self;
        remove_if_current(&mut self.home.lock_index(), self.hash, this);

        reclaim::route(&self.home.reclaim, bytes);
    }
}

/// Remove `hash`'s index slot only while it still points at `entry`. A
/// check-in that found the slot dead before `entry`'s drop took the lock has
/// already replaced it with a newer entry, which must stay indexed.
pub(super) fn remove_if_current(index: &mut Index, hash: BlobHash, entry: *const BlobEntry) {
    if index.get(&hash).is_some_and(|slot| ptr::eq(slot.as_ptr(), entry)) {
        index.remove(&hash);
    }
}
