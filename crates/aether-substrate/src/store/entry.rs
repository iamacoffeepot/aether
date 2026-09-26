//! A resident entry, its storage and its dedup key.

use std::fmt;
use std::mem;
use std::ops::Range;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use aether_data::{Blob, BlobBacking, BlobHash};

use super::slab::Slab;
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
    storage: Storage,
    /// The store state the drop path needs: the index, the byte counters and
    /// the reclaim sender.
    home: Arc<Shared>,
}

/// Where an entry's bytes live.
enum Storage {
    /// A buffer of its own, as every single check-in makes.
    Own(Box<[u8]>),
    /// A region of a slab the entry shares with the rest of one producer's
    /// check-in. The slab outlives every entry over it.
    Slab { slab: Arc<Slab>, range: Range<usize> },
}

impl BlobEntry {
    /// An entry that owns `bytes`. The caller counts them as resident.
    pub(super) fn own(hash: BlobHash, bytes: Box<[u8]>, home: Arc<Shared>) -> Self {
        Self { hash, storage: Storage::Own(bytes), home }
    }

    /// An entry over `range` of `slab`, counted as a live slab member from
    /// here. Only a [`super::slab::SlabBuilder`] makes one, over a range it
    /// laid out.
    pub(super) fn slab(hash: BlobHash, slab: Arc<Slab>, range: Range<usize>, home: Arc<Shared>) -> Self {
        home.slab_member_bytes.fetch_add(range.len(), Ordering::Relaxed);
        Self { hash, storage: Storage::Slab { slab, range }, home }
    }

    /// Whether the entry's bytes are a region of a slab rather than its own
    /// buffer.
    pub(super) const fn is_slab(&self) -> bool {
        matches!(self.storage, Storage::Slab { .. })
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match &self.storage {
            Storage::Own(bytes) => bytes,
            Storage::Slab { slab, range } => &slab.bytes()[range.clone()],
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match &self.storage {
            Storage::Own(bytes) => bytes.len(),
            Storage::Slab { range, .. } => range.len(),
        }
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
        f.debug_struct("BlobEntry").field("hash", &self.hash).field("len", &self.len()).finish_non_exhaustive()
    }
}

impl BlobBacking for BlobEntry {
    fn len(&self) -> u64 {
        self.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize {
        let Some(rest) = usize::try_from(offset).ok().and_then(|start| self.bytes().get(start..)) else {
            return 0;
        };
        let copied = rest.len().min(buf.len());
        buf[..copied].copy_from_slice(&rest[..copied]);
        copied
    }
}

impl Drop for BlobEntry {
    /// An owned entry gives back and frees its bytes. A slab entry gives back
    /// only its live-member count; its `Arc<Slab>` field drops after this
    /// returns, once the index guard is gone, and frees the slab when it is
    /// the last one.
    fn drop(&mut self) {
        let owned = match &mut self.storage {
            Storage::Own(bytes) => {
                let bytes = mem::take(bytes);
                self.home.resident_bytes.fetch_sub(bytes.len(), Ordering::Relaxed);
                Some(bytes)
            }
            Storage::Slab { range, .. } => {
                self.home.slab_member_bytes.fetch_sub(range.len(), Ordering::Relaxed);
                None
            }
        };

        let this: *const Self = self;
        remove_if_current(&mut self.home.lock_index(), self.hash, this);

        if let Some(bytes) = owned {
            reclaim::route(&self.home.reclaim, bytes);
        }
    }
}

/// Remove `hash`'s index slot only while it still points at `entry`. A
/// check-in that found the slot dead before `entry`'s drop took the lock has
/// already replaced it with a newer entry, which must stay indexed, and an
/// owned check-in may have taken the slot from a live slab entry.
pub(super) fn remove_if_current(index: &mut Index, hash: BlobHash, entry: *const BlobEntry) {
    if index.get(&hash).is_some_and(|slot| ptr::eq(slot.as_ptr(), entry)) {
        index.remove(&hash);
    }
}
