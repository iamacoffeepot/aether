//! A resident entry and its dedup key.

use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{Index, Shared, reclaim};

/// The BLAKE3 digest of an entry's bytes: the dedup index's key and nothing
/// more. It has no public constructor, and knowing one grants no access to
/// the bytes it names (ADR-0238 decision 4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlobHash([u8; blake3::OUT_LEN]);

impl BlobHash {
    pub(super) fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
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

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[must_use]
    pub fn hash(&self) -> BlobHash {
        self.hash
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
