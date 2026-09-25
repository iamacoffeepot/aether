//! One wasm instance's blob table (ADR-0238 decisions 2 and 4): each store
//! entry the instance's guest holds, keyed by its hash, with the count of live
//! `GuestHold`s over it.
//!
//! - **What a count is.** Delivery adds one count per `Blob` field it hands the
//!   guest, and the guest's `blob_drop_p32` (a `GuestHold`'s drop) gives one
//!   back. At zero the entry leaves the table and its `Arc` drops.
//! - **Resolving a hash.** The `blob_*_p32` host fns resolve a guest-supplied
//!   hash only here, so a guessed or logged hash reaches nothing this instance
//!   does not already hold.
//! - **No lock.** Only the owning actor's handler touches the table. It lives
//!   in the `Store` data (`ComponentCtx`) that the slot's actor `Mutex`
//!   guards, one handler at a time.
//! - **One instance.** A `replace` builds the new instance a fresh
//!   `ComponentCtx`, so its table starts empty, and the old table drops with
//!   the old instance. Counts for references that died with the old
//!   instance's memory cannot outlive it.
//! - **Teardown.** The engine frees a guest's memory without running its
//!   `Drop`s, so entries still counted at teardown are normal for a guest that
//!   keeps blobs in its state. Dropping the table releases them and logs one
//!   debug-level count, not a leak warning.
//! - **Saturation.** A count saturates rather than wrapping, and a saturated
//!   count never falls back to zero, so a lost grant keeps an entry rather
//!   than freeing one still referenced.
//!
//! Releasing drops an `Arc<BlobEntry>` on the handler thread, never under the
//! store's index lock, which `BlobEntry::drop` takes.

use std::num::NonZeroU64;
use std::sync::Arc;

use aether_data::BlobHash;
use rustc_hash::FxHashMap;

use crate::store::BlobEntry;

/// The store entries one wasm instance holds, keyed by hash. See the module
/// docs.
#[derive(Default)]
pub struct BlobTable {
    held: FxHashMap<BlobHash, Held>,
}

/// One held entry and its count of live guest holds.
struct Held {
    entry: Arc<BlobEntry>,
    count: NonZeroU64,
}

/// The table holds no count for the hash.
#[derive(Debug)]
pub struct NotHeld;

impl BlobTable {
    /// Add one count for `entry`, inserting it at one. The test seam for
    /// delivery's install, which un-gates it.
    #[cfg(test)]
    pub fn grant(&mut self, entry: Arc<BlobEntry>) {
        self.held
            .entry(entry.hash())
            .and_modify(|held| held.count = held.count.saturating_add(1))
            .or_insert(Held { entry, count: NonZeroU64::MIN });
    }

    /// Give back one count for `hash`. At zero the entry leaves the table and
    /// its `Arc` drops here. A saturated count stays saturated.
    pub fn release(&mut self, hash: BlobHash) -> Result<(), NotHeld> {
        let held = self.held.get_mut(&hash).ok_or(NotHeld)?;
        if held.count == NonZeroU64::MAX {
            return Ok(());
        }
        match NonZeroU64::new(held.count.get() - 1) {
            Some(count) => held.count = count,
            None => drop(self.held.remove(&hash)),
        }
        Ok(())
    }

    /// The entry `hash` names, when this instance holds it.
    pub fn entry(&self, hash: BlobHash) -> Option<&Arc<BlobEntry>> {
        self.held.get(&hash).map(|held| &held.entry)
    }
}

impl Drop for BlobTable {
    fn drop(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let holds = self.held.values().fold(0u64, |sum, held| sum.saturating_add(held.count.get()));
        tracing::debug!(
            target: "aether_substrate::component",
            entries = self.held.len(),
            holds,
            "releasing blob entries still held at instance teardown",
        );
    }
}
