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
//!
//! The thread itself is spawned from the wake, with
//! [`SelfWake::spawn_sidecar`]. That is the sanctioned spawn for a cap's
//! dedicated thread: a panic on it fails the chassis fast (ADR-0063), even
//! after the actor has closed, because the chassis aborter is taken strongly
//! at spawn time and never handed to the cap.

use std::io;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};

use aether_data::ActorMail;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::offload::fail_fast;

/// Wakes the actor that minted it with one `K`, from any thread.
///
/// Minted by `ctx.self_wake::<K>()` on
/// [`NativeInitCtx`](crate::actor::native::NativeInitCtx) and
/// [`NativeCtx`](crate::actor::native::NativeCtx). Cloning it is cheap, and
/// every clone wakes the same actor.
///
/// The thread that holds it is spawned with [`Self::spawn_sidecar`], so a
/// panic on that thread is fatal like a handler panic.
pub struct SelfWake<K> {
    binding: Weak<NativeBinding>,
    _kind: PhantomData<fn(&K)>,
}

impl<K> SelfWake<K> {
    pub(crate) fn new(binding: &Arc<NativeBinding>) -> Self {
        Self { binding: Arc::downgrade(binding), _kind: PhantomData }
    }

    /// Spawn the cap's dedicated sidecar thread — an accept loop, a socket
    /// reader, a one-shot dial — named `name`, running `body`.
    ///
    /// A panic in `body` is fatal (ADR-0063): it escalates through the chassis
    /// aborter with the panic payload in the reason, exactly as a handler
    /// panic does, and it does so even after the actor this wake names has
    /// closed. The thread gets nothing from the substrate beyond what `body`
    /// captures; its link to the actor stays this one-kind wake.
    ///
    /// # Errors
    ///
    /// Fails when the actor this wake names has already closed, or when the
    /// OS refuses the thread.
    #[allow(clippy::disallowed_methods)] // aether-suppression-request: the sanctioned sidecar spawn; the lint points cap threads here
    pub fn spawn_sidecar<F>(&self, name: String, body: F) -> io::Result<JoinHandle<()>>
    where
        F: FnOnce() + Send + 'static,
    {
        let aborter = self
            .binding
            .upgrade()
            .ok_or_else(|| io::Error::other("the actor this wake names has closed"))?
            .fatal_aborter();
        let site = format!("sidecar thread {name}");

        thread::Builder::new().name(name).spawn(move || fail_fast::run_or_abort(aborter.as_ref(), &site, body))
    }
}

impl<K: ActorMail> SelfWake<K> {
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use aether_data::MailboxId;

    use super::*;
    use crate::mail::Mailer;
    use crate::mail::registry::Registry;
    use crate::runtime::panic_hook::payload_string;

    /// A sidecar that outlives its actor still fails fast: the aborter is
    /// taken at spawn time, so the panic reaches it after the binding and
    /// every wake are gone. The test binding's aborter is `PanicAborter`, so
    /// the joined payload is the aborter's own message.
    #[test]
    fn sidecar_panic_escalates_after_the_actor_closed() {
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x6431)));
        let wake = SelfWake::<aether_kinds::Tick>::new(&binding);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();

        let sidecar = wake
            .spawn_sidecar(String::from("probe-6431"), move || {
                let _ = gate_rx.recv();
                panic!("sidecar probe 6431");
            })
            .expect("the actor is live at spawn time");
        drop(binding);
        drop(wake);
        gate_tx.send(()).expect("release the sidecar");
        let payload = sidecar.join().expect_err("the sidecar panics");
        let reason = payload_string(payload.as_ref());

        assert!(reason.contains("fatal abort"), "the aborter ran: {reason}");
        assert!(reason.contains("sidecar thread probe-6431"), "reason names the site: {reason}");
        assert!(reason.contains("sidecar probe 6431"), "reason carries the payload: {reason}");
    }
}
