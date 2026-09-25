//! Checking bytes into the engine blob store (ADR-0238 decisions 1, 2 and 9).
//!
//! The ctx is the only route to this engine's store, so the verb lives here.
//! It takes `&self`: the store is internally synchronized and a native actor
//! keeps no table, so check-in mutates nothing in the ctx.

use aether_actor::ReplyMode;
use aether_data::BlobRef;

use super::NativeCtx;

impl<A, M: ReplyMode> NativeCtx<'_, A, M> {
    /// Check `bytes` into the engine blob store and hold a reference to them.
    /// Equal bytes are resident once. The bytes stay resident while any
    /// `BlobRef` to them lives.
    #[must_use]
    pub fn check_in(&self, bytes: Box<[u8]>) -> BlobRef {
        self.binding.mailer().blob_store().check_in(bytes).into_ref()
    }
}
