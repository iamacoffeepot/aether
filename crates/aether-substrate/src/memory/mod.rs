//! The engine's memory ledger: who holds how many bytes, written by each
//! owner at the moment its bytes change and read by one report.
//!
//! A [`MemoryGauge`] is one row: the actor that owns it and a short label
//! saying what the bytes are. An actor mints one from its init ctx
//! ([`NativeInitCtx::memory_gauge`](crate::NativeInitCtx::memory_gauge)), and
//! dropping it removes the row, so a departed actor leaves none behind. An
//! owner whose bytes are one number sets the gauge to it
//! ([`MemoryGauge::set`]); an owner whose bytes are many resources hands each
//! a [`MemoryCharge`], which adds on creation, can be resized, and subtracts
//! when dropped, so the bytes leave the count wherever the resource is
//! dropped.
//!
//! The ledger is held by the [`Mailer`](crate::Mailer) beside the cost table
//! and the blob store, so it is reached through a ctx and is never a global.
//! Nothing here is on mail dispatch: a charge or a set is one relaxed atomic
//! operation, and the ledger's lock is taken only when a gauge is minted or
//! dropped (actor birth and death) and when a report is built.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::mail::MailboxId;

mod process;
#[cfg(test)]
mod tests;

pub(crate) use process::resident_set_bytes;

/// One live gauge as the ledger lists it.
struct Entry {
    /// The gauge's own id, which its drop removes the entry by.
    id: u64,
    owner: MailboxId,
    label: &'static str,
    bytes: Arc<AtomicUsize>,
}

/// One owner's bytes under one label at the moment the ledger was read.
pub(crate) struct LedgerRow {
    pub(crate) owner: MailboxId,
    pub(crate) label: &'static str,
    pub(crate) bytes: usize,
}

/// The engine's one list of live gauges.
#[derive(Default)]
pub(crate) struct MemoryLedger {
    entries: Mutex<Vec<Entry>>,
    next_id: AtomicU64,
}

impl MemoryLedger {
    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        // No operation leaves the list half-updated, so a poisoned lock still
        // guards a consistent list.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mint a gauge at zero bytes for `owner`, listed until it drops.
    pub(crate) fn gauge(self: &Arc<Self>, owner: MailboxId, label: &'static str) -> MemoryGauge {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let bytes = Arc::new(AtomicUsize::new(0));
        self.lock().push(Entry { id, owner, label, bytes: Arc::clone(&bytes) });

        MemoryGauge { bytes, listing: Some(Listing { ledger: Arc::clone(self), id }) }
    }

    /// One row per owner and label. Two gauges one owner holds under one
    /// label, as a guest and its replacement do while a republish prepares,
    /// are one row with their sum.
    pub(crate) fn rows(&self) -> Vec<LedgerRow> {
        let mut totals: BTreeMap<(MailboxId, &'static str), usize> = BTreeMap::new();
        for entry in self.lock().iter() {
            let total = totals.entry((entry.owner, entry.label)).or_default();
            *total = total.saturating_add(entry.bytes.load(Ordering::Relaxed));
        }

        totals.into_iter().map(|((owner, label), bytes)| LedgerRow { owner, label, bytes }).collect()
    }
}

/// A listed gauge's place in its ledger.
struct Listing {
    ledger: Arc<MemoryLedger>,
    id: u64,
}

/// One owner's byte count under one label. Listed in the engine's memory
/// report from the moment it is minted until it drops.
pub struct MemoryGauge {
    bytes: Arc<AtomicUsize>,
    /// `None` for a detached gauge, which no ledger lists.
    listing: Option<Listing>,
}

impl MemoryGauge {
    /// A gauge no ledger lists: it counts as a listed one does and appears in
    /// no report. For a registry built outside a running engine, as a test
    /// builds one.
    #[must_use]
    pub fn detached() -> Self {
        Self { bytes: Arc::new(AtomicUsize::new(0)), listing: None }
    }

    /// Set this gauge to `bytes`. For an owner whose memory is one number it
    /// learns whole, as a wasm linear memory's size is; it is never mixed
    /// with [`Self::charge`] on one gauge.
    pub fn set(&self, bytes: usize) {
        self.bytes.store(bytes, Ordering::Relaxed);
    }

    /// Add `bytes` to this gauge for as long as the returned charge lives.
    #[must_use]
    pub fn charge(&self, bytes: usize) -> MemoryCharge {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        MemoryCharge { gauge: Arc::clone(&self.bytes), bytes }
    }

    /// The bytes this gauge counts now.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// A default gauge is a detached one, so a registry that derives `Default`
/// agrees with its own detached constructor.
impl Default for MemoryGauge {
    fn default() -> Self {
        Self::detached()
    }
}

impl Drop for MemoryGauge {
    fn drop(&mut self) {
        if let Some(listing) = &self.listing {
            listing.ledger.lock().retain(|entry| entry.id != listing.id);
        }
    }
}

/// Some bytes counted on a [`MemoryGauge`] for as long as this value lives.
/// The resource that occupies the bytes holds it as a field, so the count
/// falls wherever the resource is dropped.
pub struct MemoryCharge {
    gauge: Arc<AtomicUsize>,
    bytes: usize,
}

impl MemoryCharge {
    /// Change this charge to `bytes`, moving its gauge by the difference.
    pub fn resize(&mut self, bytes: usize) {
        if bytes >= self.bytes {
            self.gauge.fetch_add(bytes - self.bytes, Ordering::Relaxed);
        } else {
            self.gauge.fetch_sub(self.bytes - bytes, Ordering::Relaxed);
        }
        self.bytes = bytes;
    }

    /// The bytes this charge counts.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        self.gauge.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// The blob store's three byte counts. `slab_bytes` is part of
/// `resident_bytes`, and `slab_member_bytes` is the live part of
/// `slab_bytes`, so the three are never summed.
#[derive(Clone, Copy, Debug)]
pub struct BlobStoreMemory {
    /// Every owned entry's bytes plus every live slab.
    pub resident_bytes: u64,
    /// Every live slab's bytes.
    pub slab_bytes: u64,
    /// Every live slab entry's bytes.
    pub slab_member_bytes: u64,
}

/// One owner's bytes under one label.
#[derive(Clone, Debug)]
pub struct OwnerMemory {
    /// The owner's actor path, or its tagged id text when the registry holds
    /// no name for it.
    pub owner: String,
    /// What the bytes are, as the owner's gauge was labelled.
    pub label: &'static str,
    pub bytes: u64,
}

/// What the engine holds, by owner, at the moment it was asked.
#[derive(Clone, Debug)]
pub struct MemoryReport {
    /// The process's resident set size, or `None` on a platform with no
    /// reader.
    pub process_bytes: Option<u64>,
    pub blob_store: BlobStoreMemory,
    /// One row per owner and label, sorted by owner then label.
    pub owners: Vec<OwnerMemory>,
}

/// `bytes` as the report carries it.
pub(crate) fn report_bytes(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX)
}
