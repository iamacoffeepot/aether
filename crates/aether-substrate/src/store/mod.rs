//! The engine blob store (ADR-0238 decisions 1, 7 and 8): one native,
//! in-memory owner of immutable checked-in bytes. It is never a mailbox.
//!
//! - **In memory only.** Nothing backs an entry with a file, so a restart
//!   forgets every blob.
//! - **Check-in only.** `BlobStore::check_in` takes ownership of a buffer
//!   and returns a shared [`BlobEntry`]. Bytes never change after check-in; a
//!   change is a new check-in. Reading an entry's bytes takes no lock.
//! - **Deduplicated by BLAKE3.** Check-in hashes the bytes before it locks
//!   the dedup index. When a live entry with that hash is resident, check-in
//!   returns it and frees the new buffer, so equal bytes are resident once.
//!   The one exception is below: an owned check-in does not adopt a slab
//!   entry.
//! - **The hash grants nothing.** An entry's identity is an
//!   [`aether_data::BlobHash`], and nothing here looks an entry up by hash.
//!
//! # Two storage forms
//!
//! An entry's bytes are either its own buffer or a region of a slab.
//!
//! - **Own.** `BlobStore::check_in` makes an entry that owns the buffer it
//!   was handed. Every single check-in is this form.
//! - **Slab.** `BlobStore::slab` allocates one buffer at the exact total of
//!   the lengths a producer declares, lets it fill each region in place, and
//!   interns each region as its own entry with its own hash and dedup slot. A
//!   producer chooses it when its members live and die together.
//!
//! The dedup slot prefers owned storage. An owned check-in that finds a live
//! slab entry with its hash makes a new owned entry and moves the slot to it:
//! existing holders keep the slab entry, and later check-ins get the owned
//! one, so a slab stays pinned only by the producer that chose it. Two live
//! entries with one hash are sound, because the hash is the identity. A slab
//! region whose hash is already resident, in either form, reuses the
//! resident entry.
//!
//! The cost a slab's producer accepts is retention: one live member keeps
//! the whole slab resident, including regions dedup made redundant. The store
//! keeps three byte counts. `resident_bytes` is every owned entry's bytes
//! plus every live slab. `slab_bytes` is every live slab, and
//! `slab_member_bytes` is every live slab entry, so their difference is what
//! slabs retain for regions no live entry uses. The gauge reports all three.
//!
//! # How actors reach it
//!
//! The engine's one store is owned by its `Mailer`. A native handler checks
//! bytes in through `NativeCtx::check_in`, which holds the resulting entry as
//! a `Shared` [`aether_data::Blob`]: the entry's `Arc` behind the
//! [`aether_data::BlobBacking`] this module implements. `BlobEntry::into_blob`
//! is the only place that mints one, so a `Shared` value always comes from
//! this store. Reads stream through `BlobBacking::read_at` and never take a
//! lock. Cloning the value adds a strong reference and dropping it lets one
//! go; native actors keep no table of their own.
//!
//! A handler whose ADR-0093 worker reads bytes off the dispatcher hands that
//! worker a `BlobCheckIn` from `NativeCtx::blob_check_in`, so the worker checks
//! the bytes in where it read them. The handle holds a clone of this store and
//! does nothing but check in, one buffer at a time or as one slab.
//!
//! # How entries are freed
//!
//! The index holds [`Weak`] references, so it never keeps bytes alive: an
//! entry is freed exactly when its last strong `Arc<BlobEntry>` drops. Its
//! `Drop` subtracts its length from its counter and removes its own index
//! slot, but only while the slot still points at this entry: a concurrent
//! check-in of the same hash may already have found the slot dead and
//! replaced it with a newer entry, or an owned check-in may have taken it
//! from a slab entry. An owned entry's buffer is then freed. A slab entry
//! lets go of its slab, which is freed, and leaves the resident count, when
//! its last entry drops or when an unfinished builder does. A buffer of at
//! least [`RECLAIM_THRESHOLD_BYTES`], owned or slab, is sent to the
//! `aether-blob-reclaim` thread and freed there, so a dispatch thread never
//! pays to unmap a large block; a smaller buffer is freed inline.
//!
//! The store never drops a referenced entry. Under memory pressure it grows,
//! and a resident-byte gauge warns once at each new high-water mark, starting
//! at [`RESIDENT_WARNING_START_BYTES`] and doubling after each.
//!
//! # Lock discipline
//!
//! No strong `Arc<BlobEntry>` may drop while the index lock is held, because
//! `BlobEntry::drop` takes that lock and [`Mutex`] is not reentrant. Dropping a
//! dead `Weak` under the lock is fine. A slab's drop never takes the lock, and
//! a slab entry's `Arc<Slab>` drops only after the entry's own drop has
//! released it.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use aether_data::BlobHash;
use rustc_hash::FxHashMap;

mod entry;
mod gauge;
mod reclaim;
mod slab;
#[cfg(test)]
mod tests;

pub use entry::BlobEntry;
pub use slab::SlabBuilder;

/// Buffers at or above this length are freed on the reclaim thread rather
/// than on the thread that drops them.
pub const RECLAIM_THRESHOLD_BYTES: usize = 1024 * 1024;

/// The first resident-byte high-water mark the gauge warns at. Each later
/// mark is double the one before.
pub const RESIDENT_WARNING_START_BYTES: usize = 256 * 1024 * 1024;

/// The dedup index: each resident hash to its entry, held weakly.
type Index = FxHashMap<BlobHash, Weak<BlobEntry>>;

/// The in-memory store of immutable checked-in bytes. See the module docs.
///
/// A clone is another handle on the same store.
#[derive(Clone)]
pub struct BlobStore {
    shared: Arc<Shared>,
}

/// State the store and every entry share. An entry keeps it alive, and so
/// does a live slab, so the reclaim thread outlives the last entry and the
/// last slab as well as the last store.
struct Shared {
    index: Mutex<Index>,
    /// Every owned entry's bytes plus every live slab.
    resident_bytes: AtomicUsize,
    /// Every live slab's bytes.
    slab_bytes: AtomicUsize,
    /// Every live slab entry's bytes.
    slab_member_bytes: AtomicUsize,
    /// The next resident-byte mark the gauge warns at.
    next_warning_bytes: AtomicUsize,
    reclaim: Sender<Box<[u8]>>,
}

impl Shared {
    fn lock_index(&self) -> MutexGuard<'_, Index> {
        // No index operation leaves the map half-updated, so a poisoned lock
        // still guards a consistent map.
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl BlobStore {
    /// A new, empty store. Spawns its reclaim thread, so a thread the OS
    /// refuses surfaces here, at boot.
    pub fn new() -> io::Result<Self> {
        let shared = Shared {
            index: Mutex::new(Index::default()),
            resident_bytes: AtomicUsize::new(0),
            slab_bytes: AtomicUsize::new(0),
            slab_member_bytes: AtomicUsize::new(0),
            next_warning_bytes: AtomicUsize::new(RESIDENT_WARNING_START_BYTES),
            reclaim: reclaim::spawn()?,
        };
        Ok(Self { shared: Arc::new(shared) })
    }

    /// Check `bytes` in as an owned entry, returning the resident entry for
    /// their hash: the already-resident one when a live owned entry has it
    /// (the new buffer is then freed), otherwise a new entry. A live slab
    /// entry with the hash keeps its holders but loses the dedup slot to the
    /// new entry.
    pub(crate) fn check_in(&self, bytes: Box<[u8]>) -> Arc<BlobEntry> {
        let hash = entry::hash_of(&bytes);

        let mut index = self.shared.lock_index();
        let displaced = match resident(&index, hash) {
            Some(found) if !found.is_slab() => {
                drop(index);
                reclaim::route(&self.shared.reclaim, bytes);
                return found;
            }
            found => found,
        };
        let len = bytes.len();
        let entry = Arc::new(BlobEntry::own(hash, bytes, Arc::clone(&self.shared)));
        index.insert(hash, Arc::downgrade(&entry));
        drop(index);
        // The displaced slab entry may be its last strong reference, so it
        // drops only now that the guard is gone.
        drop(displaced);

        let resident = self.shared.resident_bytes.fetch_add(len, Ordering::Relaxed) + len;
        gauge::observe(&self.shared, resident);
        entry
    }

    /// A builder for one slab of exactly the sum of `lens`, with one region
    /// per length. See the module docs for what a slab costs.
    ///
    /// # Panics
    ///
    /// As `vec!` does, when the total cannot be allocated.
    #[must_use]
    pub(crate) fn slab(&self, lens: &[usize]) -> SlabBuilder {
        SlabBuilder::new(lens, Arc::clone(&self.shared))
    }

    /// The total length of every owned entry's bytes plus every live slab,
    /// each counted once. Read only by tests until the gauge has a reader
    /// outside the store.
    #[cfg(test)]
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.shared.resident_bytes.load(Ordering::Relaxed)
    }

    /// The total length of every live slab. Read only by tests.
    #[cfg(test)]
    #[must_use]
    pub fn slab_bytes(&self) -> usize {
        self.shared.slab_bytes.load(Ordering::Relaxed)
    }

    /// The total length of every live slab entry's region. Read only by tests.
    #[cfg(test)]
    #[must_use]
    pub fn slab_member_bytes(&self) -> usize {
        self.shared.slab_member_bytes.load(Ordering::Relaxed)
    }
}

/// The live entry `hash`'s slot points at, if any. The caller holds the index
/// lock, so it must keep the returned `Arc` past the guard rather than drop it
/// there.
fn resident(index: &Index, hash: BlobHash) -> Option<Arc<BlobEntry>> {
    index.get(&hash).and_then(Weak::upgrade)
}
