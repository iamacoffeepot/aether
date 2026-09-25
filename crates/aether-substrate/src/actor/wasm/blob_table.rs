//! One wasm instance's blob table (ADR-0238 decisions 2 and 4): each store
//! entry the instance's guest can reach, keyed by its hash, with the count of
//! live `GuestHold`s over it and whether the current receive call pins it.
//!
//! - **Pins.** Delivery pins each entry the inbound mail attaches, for the
//!   length of one `receive_p32` call, and drops every pin when the call
//!   returns, on success and on a trap alike. A pin admits holds; it is not a
//!   hold, so a field the guest never decodes holds nothing past the call.
//! - **Holds.** Building a guest `Blob` takes one hold (`blob_hold_p32`),
//!   which a pin or an existing hold admits, and the value's `GuestHold` drop
//!   gives it back (`blob_drop_p32`). Holds count live values, however many
//!   times the guest decodes one mail. An entry leaves the table, and its
//!   `Arc` drops, once it is neither pinned nor held.
//! - **Resolving a hash.** The `blob_*_p32` host fns resolve a guest-supplied
//!   hash only here, so a guessed or logged hash reaches nothing this instance
//!   is not already pinned or holding.
//! - **No lock.** Only the owning actor's handler touches the table. It lives
//!   in the `Store` data (`ComponentCtx`) that the slot's actor `Mutex`
//!   guards, one handler at a time.
//! - **One instance.** A `replace` builds the new instance a fresh
//!   `ComponentCtx`, so its table starts empty, and the old table drops with
//!   the old instance. Holds for references that died with the old
//!   instance's memory cannot outlive it.
//! - **Teardown.** The engine frees a guest's memory without running its
//!   `Drop`s, so entries still held at teardown are normal for a guest that
//!   keeps blobs in its state. Dropping the table releases them and logs one
//!   debug-level count, not a leak warning.
//! - **Saturation.** A hold count saturates rather than wrapping, and a
//!   saturated count never falls back to zero, so a lost release keeps an
//!   entry rather than freeing one still referenced.
//!
//! Releasing drops an `Arc<BlobEntry>` on the handler thread, never under the
//! store's index lock, which `BlobEntry::drop` takes.

use std::sync::Arc;

use aether_data::BlobHash;
use rustc_hash::FxHashMap;

use crate::store::BlobEntry;

/// The store entries one wasm instance pins or holds, keyed by hash. See the
/// module docs.
#[derive(Default)]
pub struct BlobTable {
    held: FxHashMap<BlobHash, Held>,
    /// The hashes the current receive call pins, so ending the call visits
    /// only them.
    pinned: Vec<BlobHash>,
}

/// One entry, its count of live guest holds, and whether the current receive
/// call pins it. An entry that is neither pinned nor held is not in the table.
struct Held {
    entry: Arc<BlobEntry>,
    holds: u64,
    pinned: bool,
}

/// The table neither pins nor holds the hash, or, for a release, holds no
/// count to give back.
#[derive(Debug)]
pub struct NotHeld;

impl BlobTable {
    /// Admit `entry` for the current receive call: delivery's install.
    /// Idempotent per hash.
    pub fn pin(&mut self, entry: Arc<BlobEntry>) {
        let hash = entry.hash();
        let held = self.held.entry(hash).or_insert(Held { entry, holds: 0, pinned: false });
        if !held.pinned {
            held.pinned = true;
            self.pinned.push(hash);
        }
    }

    /// End the receive call: drop every pin. An entry with no holds leaves
    /// the table and its `Arc` drops here.
    pub fn unpin_all(&mut self) {
        for hash in self.pinned.drain(..) {
            let Some(held) = self.held.get_mut(&hash) else {
                continue;
            };
            held.pinned = false;
            if held.holds == 0 {
                drop(self.held.remove(&hash));
            }
        }
    }

    /// Take one more hold on a pinned or held `hash` and return the entry's
    /// length. A saturated count stays saturated.
    pub fn hold(&mut self, hash: BlobHash) -> Result<usize, NotHeld> {
        let held = self.held.get_mut(&hash).ok_or(NotHeld)?;
        held.holds = held.holds.saturating_add(1);
        Ok(held.entry.len())
    }

    /// Give back one hold on `hash`. Once the entry is neither held nor
    /// pinned it leaves the table and its `Arc` drops here. A saturated count
    /// stays saturated, and a pinned entry with no holds has none to give.
    pub fn release(&mut self, hash: BlobHash) -> Result<(), NotHeld> {
        let held = self.held.get_mut(&hash).ok_or(NotHeld)?;
        match held.holds {
            0 => return Err(NotHeld),
            u64::MAX => return Ok(()),
            _ => held.holds -= 1,
        }
        if held.holds == 0 && !held.pinned {
            drop(self.held.remove(&hash));
        }
        Ok(())
    }

    /// The entry `hash` names, when this instance pins or holds it.
    pub fn entry(&self, hash: BlobHash) -> Option<&Arc<BlobEntry>> {
        self.held.get(&hash).map(|held| &held.entry)
    }
}

impl Drop for BlobTable {
    fn drop(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let holds = self.held.values().fold(0u64, |sum, held| sum.saturating_add(held.holds));
        tracing::debug!(
            target: "aether_substrate::component",
            entries = self.held.len(),
            holds,
            "releasing blob entries still held at instance teardown",
        );
    }
}
