//! The check-in handle a hold-until-resolve worker holds in place of a ctx.
//!
//! An ADR-0093 worker gets no ctx (ADR-0080 §12), yet a worker that reads
//! large bytes off the dispatcher also has to check them in off the
//! dispatcher: hashing and interning them on the completion turn would put
//! the stall back on the handler thread. [`BlobCheckIn`] is that and nothing
//! else. It holds a clone of the engine blob store and checks bytes in
//! exactly as `ctx.check_in` does (ADR-0238 decisions 1 and 2).
//!
//! It grants nothing else. It sends no mail, names no position, looks no
//! entry up by hash, and exposes no mailer or binding, so the worker that
//! holds it still sends nothing (ADR-0080 §12). The value it returns is the
//! same `Shared` [`Blob`] a handler would get, minted by the store's one mint
//! site.

use aether_data::Blob;

use crate::store::BlobStore;

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
}
