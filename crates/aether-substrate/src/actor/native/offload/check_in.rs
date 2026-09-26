//! The check-in handle a hold-until-resolve worker holds in place of a ctx.
//!
//! An ADR-0093 worker gets no ctx (ADR-0080 §12), yet a worker that reads
//! large bytes off the dispatcher also has to check them in off the
//! dispatcher: hashing and interning them on the completion turn would put
//! the stall back on the handler thread. [`BlobCheckIn`] is that and nothing
//! else. It holds a clone of the engine blob store and checks bytes in
//! exactly as `ctx.check_in` does (ADR-0238 decisions 1 and 2), or, through
//! [`BlobCheckIn::slab`], checks a set of members in as one slab
//! (ADR-0238 decision 8).
//!
//! It grants nothing else. It sends no mail, names no position, looks no
//! entry up by hash, and exposes no mailer or binding, so the worker that
//! holds it still sends nothing (ADR-0080 §12). Every value it returns is the
//! same `Shared` [`Blob`] a handler would get, minted by the store's one mint
//! site.

use aether_data::Blob;

use crate::store::{BlobEntry, BlobStore, SlabBuilder};

/// Checks bytes into the engine blob store from any thread.
///
/// Minted by `ctx.blob_check_in()` on
/// [`NativeCtx`](crate::actor::native::NativeCtx), for a hold-until-resolve
/// worker to move into its closure. Every value it returns is a `Shared`
/// [`Blob`] in the store of the engine that minted it.
pub struct BlobCheckIn {
    store: BlobStore,
}

impl BlobCheckIn {
    pub(crate) fn new(store: BlobStore) -> Self {
        Self { store }
    }

    /// Check `bytes` into the engine blob store and hold them as a `Shared`
    /// [`Blob`], as `ctx.check_in` does. Equal bytes are resident once. The
    /// bytes stay resident while any clone of the value lives.
    #[must_use]
    pub fn check_in(&self, bytes: Box<[u8]>) -> Blob {
        self.store.check_in(bytes).into_blob()
    }

    /// Start checking in one member per length in `lens` as a single slab:
    /// one allocation of exactly their total, which the caller fills region
    /// by region in place before [`BlobSlab::finish`].
    ///
    /// Choose it only for members that live and die together. Each member is
    /// still its own `Shared` [`Blob`] with its own hash, but one live member
    /// keeps the whole slab resident, including a region whose bytes were
    /// already resident and so were not used.
    ///
    /// # Panics
    ///
    /// As `vec!` does, when the total cannot be allocated.
    #[must_use]
    pub fn slab(&self, lens: &[usize]) -> BlobSlab {
        BlobSlab { builder: self.store.slab(lens) }
    }
}

/// One slab being filled, from [`BlobCheckIn::slab`]. The regions are
/// exactly the declared lengths, back to back, so none can overlap or run
/// out of bounds. Dropping it before [`BlobSlab::finish`] frees the slab and
/// checks nothing in.
pub struct BlobSlab {
    builder: SlabBuilder,
}

impl BlobSlab {
    /// Each declared region, in declared order, for the caller to fill.
    pub fn regions(&mut self) -> impl Iterator<Item = &mut [u8]> {
        self.builder.regions()
    }

    /// Check every region in and hold each as a `Shared` [`Blob`]: exactly
    /// one value per declared length, in declared order. A region whose bytes
    /// are already resident returns the resident value.
    #[must_use]
    pub fn finish(self) -> Vec<Blob> {
        self.builder.finish().into_iter().map(BlobEntry::into_blob).collect()
    }
}
