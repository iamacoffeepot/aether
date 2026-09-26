//! Checking bytes into the engine blob store (ADR-0238 decisions 1, 2 and 9).
//!
//! The ctx is the only route to this engine's store, so the verbs live here.
//! [`NativeCtx::check_in`] checks bytes in on the handler thread;
//! [`NativeCtx::blob_check_in`] mints the [`BlobCheckIn`] handle an ADR-0093
//! worker holds to check bytes in off the dispatcher. Both take `&self`: the
//! store is internally synchronized and a native actor keeps no table, so
//! check-in mutates nothing in the ctx.

use aether_actor::ReplyMode;
use aether_data::Blob;

use super::NativeCtx;
use crate::actor::native::offload::check_in::BlobCheckIn;

impl<A, M: ReplyMode> NativeCtx<'_, A, M> {
    /// Check `bytes` into the engine blob store and hold them as a `Shared`
    /// [`Blob`]. Equal bytes are resident once. The bytes stay resident while
    /// any clone of the value lives.
    #[must_use]
    pub fn check_in(&self, bytes: Box<[u8]>) -> Blob {
        self.binding.mailer().blob_store().check_in(bytes).into_blob()
    }

    /// A handle that checks bytes into this engine's blob store from any
    /// thread, for a hold-until-resolve worker that reads bytes off the
    /// dispatcher and must check them in there too. It checks in exactly as
    /// [`Self::check_in`] does and grants nothing else.
    #[must_use]
    pub fn blob_check_in(&self) -> BlobCheckIn {
        BlobCheckIn::new(self.binding.mailer().blob_store().clone())
    }
}
