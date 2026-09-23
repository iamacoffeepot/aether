//! The self-wake handle an off-thread helper holds in place of a position.
//!
//! A cap that runs its own thread — an accept loop, a socket reader, a timer —
//! needs exactly one thing from the actor it serves: a way to wake it so the
//! next handler turn drains whatever the thread staged. [`SelfWake<K>`] is that
//! and nothing else. It names no position, carries no reference, and exposes
//! no mailer or binding, so the only send it grants is one self-wake of `K`
//! (ADR-0230): the helper cannot address any other actor through it.
//!
//! The wake is the same unchained loopback push the ADR-0093 completion tail
//! makes for [`TaskCompletionWake`](super::blocking::TaskCompletionWake). The
//! handle holds its binding weakly, so a wake after the actor has dropped does
//! nothing.

use std::marker::PhantomData;
use std::sync::{Arc, Weak};

use aether_data::Kind;

use crate::actor::native::binding::NativeBinding;

/// Wakes the actor that minted it with one `K`, from any thread.
///
/// Minted by `ctx.self_wake::<K>()` on
/// [`NativeInitCtx`](crate::actor::native::NativeInitCtx) and
/// [`NativeCtx`](crate::actor::native::NativeCtx). Cloning it is cheap, and
/// every clone wakes the same actor.
pub struct SelfWake<K> {
    binding: Weak<NativeBinding>,
    _kind: PhantomData<fn(&K)>,
}

impl<K> SelfWake<K> {
    pub(crate) fn new(binding: &Arc<NativeBinding>) -> Self {
        Self { binding: Arc::downgrade(binding), _kind: PhantomData }
    }
}

impl<K: Kind> SelfWake<K> {
    /// Push `payload` to the minting actor's own mailbox as an unchained
    /// loopback wake. Does nothing once that actor has dropped.
    pub fn wake(&self, payload: &K) {
        if let Some(binding) = self.binding.upgrade() {
            binding.wake_self(K::ID, payload.encode_into_bytes());
        }
    }
}

impl<K> Clone for SelfWake<K> {
    fn clone(&self) -> Self {
        Self { binding: Weak::clone(&self.binding), _kind: PhantomData }
    }
}
