//! The engine blob store (ADR-0238 decisions 1, 7 and 8): one native,
//! in-memory owner of immutable checked-in bytes. It is never a mailbox.
//!
//! - **In memory only.** Nothing backs an entry with a file, so a restart
//!   forgets every blob.
//! - **Check-in only.** [`BlobStore::check_in`] takes ownership of a buffer
//!   and returns a shared [`BlobEntry`]. Bytes never change after check-in; a
//!   change is a new check-in. Reading an entry's bytes takes no lock.
//! - **Deduplicated by BLAKE3.** Check-in hashes the bytes before it locks
//!   the dedup index. When a live entry with that hash is resident, check-in
//!   returns it and frees the new buffer, so equal bytes are resident once.
//! - **The hash grants nothing.** [`BlobHash`] is a dedup key with no public
//!   constructor, and nothing here looks an entry up by hash.
//!
//! # How entries are freed
//!
//! The index holds [`Weak`] references, so it never keeps bytes alive: an
//! entry is freed exactly when its last strong `Arc<BlobEntry>` drops. Its
//! `Drop` subtracts its length from the resident-byte counter and removes its
//! own index slot, but only while the slot still points at this entry: a
//! concurrent check-in of the same hash may already have found the slot dead
//! and replaced it with a newer entry. A buffer of at least
//! [`RECLAIM_THRESHOLD_BYTES`] is then sent to the `aether-blob-reclaim`
//! thread and freed there, so a dispatch thread never pays to unmap a large
//! block; a smaller buffer is freed inline.
//!
//! The store never drops a referenced entry. Under memory pressure it grows,
//! and a resident-byte gauge warns once at each new high-water mark, starting
//! at [`RESIDENT_WARNING_START_BYTES`] and doubling after each.
//!
//! # Lock discipline
//!
//! No strong `Arc<BlobEntry>` may drop while the index lock is held, because
//! `BlobEntry::drop` takes that lock and [`Mutex`] is not reentrant. Dropping a
//! dead `Weak` under the lock is fine.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use rustc_hash::FxHashMap;

mod entry;
mod gauge;
mod reclaim;
#[cfg(test)]
mod tests;

pub use entry::{BlobEntry, BlobHash};

/// Buffers at or above this length are freed on the reclaim thread rather
/// than on the thread that drops them.
pub const RECLAIM_THRESHOLD_BYTES: usize = 1024 * 1024;

/// The first resident-byte high-water mark the gauge warns at. Each later
/// mark is double the one before.
pub const RESIDENT_WARNING_START_BYTES: usize = 256 * 1024 * 1024;

/// The dedup index: each resident hash to its entry, held weakly.
type Index = FxHashMap<BlobHash, Weak<BlobEntry>>;

/// The in-memory store of immutable checked-in bytes. See the module docs.
pub struct BlobStore {
    shared: Arc<Shared>,
}

/// State the store and every entry share. An entry keeps it alive, so the
/// reclaim thread outlives the last entry as well as the last store.
struct Shared {
    index: Mutex<Index>,
    resident_bytes: AtomicUsize,
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
            next_warning_bytes: AtomicUsize::new(RESIDENT_WARNING_START_BYTES),
            reclaim: reclaim::spawn()?,
        };
        Ok(Self { shared: Arc::new(shared) })
    }

    /// Check `bytes` in, returning the resident entry for their hash: the
    /// already-resident one when a live entry has it (the new buffer is then
    /// freed), otherwise a new entry.
    pub fn check_in(&self, bytes: Box<[u8]>) -> Arc<BlobEntry> {
        let hash = BlobHash::of(&bytes);

        let mut index = self.shared.lock_index();
        if let Some(resident) = index.get(&hash).and_then(Weak::upgrade) {
            drop(index);
            reclaim::route(&self.shared.reclaim, bytes);
            return resident;
        }
        let len = bytes.len();
        let entry = Arc::new(BlobEntry::new(hash, bytes, Arc::clone(&self.shared)));
        index.insert(hash, Arc::downgrade(&entry));
        drop(index);

        let resident = self.shared.resident_bytes.fetch_add(len, Ordering::Relaxed) + len;
        gauge::observe(&self.shared.next_warning_bytes, resident);
        entry
    }

    /// The total length of every live entry's bytes, each counted once.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.shared.resident_bytes.load(Ordering::Relaxed)
    }
}
