//! Reading the engine's actor clock.
//!
//! The ctx is the only route to this engine's clock, so the verb lives here.
//! [`NativeCtx::now`] is the twin of the guest's `WasmCtx::now`: both read the
//! one [`ActorClock`](crate::runtime::actor_clock::ActorClock) the engine's
//! mailer holds, so a guest and a native actor in one engine measure against
//! the same clock.

use aether_actor::{Instant, ReplyMode};

use super::NativeCtx;

impl<A, S, M: ReplyMode> NativeCtx<'_, A, S, M> {
    /// A reading of the engine's actor clock. Subtract an earlier reading
    /// with [`Instant::since`] to measure how long work took: within one
    /// engine a later reading is never less than an earlier one, and only
    /// the difference means anything. An [`Instant`] has no codec, so one
    /// that outlives the handler is kept in actor state.
    ///
    /// The guest twin is `aether_actor::WasmCtx::now`, which reads the same
    /// clock.
    #[must_use]
    pub fn now(&self) -> Instant {
        aether_actor::__mint_instant(self.binding.mailer().actor_clock().now_nanos())
    }
}
