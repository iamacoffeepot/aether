//! ADR-0074 §Decision: native-side `MailTransport` implementation.
//!
//! [`NativeTransport`] is a regular struct each capability owns. It
//! holds the per-actor state — mailer + self mailbox + inbox +
//! correlation counter + wait-overflow queue — directly as fields,
//! reached via `&self` on every trait method. No thread-locals, no
//! install/uninstall ceremony, no RefCell runtime borrow checks.
//! The actor binding is type-system-tracked through the `&T`
//! references the SDK threads into `Sink::send`, `Ctx<'a, T>`, the
//! `wait_reply` helper, and the typed-handle module.
//!
//! Capabilities build their `NativeTransport` at boot and pass
//! `&self.transport` (or thread it through to a worker) wherever an
//! `&T` is needed. The wasm-guest path uses
//! `aether_component::WasmTransport` (a ZST) the same way; both
//! impls share the SDK in `aether-actor`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use aether_actor::MailTransport;

use crate::capability::Envelope;
use crate::mail::{KindId, Mail, MailboxId, ReplyTarget, ReplyTo};
use crate::mailer::Mailer;

/// Owned `MailTransport` impl for a native actor. Each capability
/// constructs one at boot via [`NativeTransport::new`] and holds it
/// for the lifetime of its dispatcher thread; SDK helpers receive
/// `&self.transport` references.
///
/// The five trait methods read/mutate the struct's fields directly:
///
/// - `send_mail` — mints a fresh correlation id (atomic
///   monotonic counter), wraps the bytes in a [`Mail`] with
///   `ReplyTarget::Component(self.self_mailbox)` so any reply
///   routes back here, and pushes through the shared
///   `Arc<Mailer>`.
/// - `reply_mail` — Phase 2b stub. Native capabilities that need
///   to reply to a sender (handle, audio, io, net) gain this when
///   they migrate; log doesn't reply.
/// - `save_state` — permanent stub. Native actors don't have a
///   `replace_component`-style hot reload path; `save_state` is a
///   wasm-component concept (ADR-0016).
/// - `wait_reply` — pulls from `self.inbox` with timeout, filters
///   by `(kind, correlation)`, parks non-matching envelopes into
///   `self.overflow` for a future `wait_reply` to find, mirrors the
///   wasm side's `SubstrateCtx::wait_reply` semantics.
/// - `prev_correlation` — reads the atomic counter.
pub struct NativeTransport {
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    /// Owned by `wait_reply`; held in a `Mutex` so the `&self`
    /// receiver can take exclusive access. Wrapped in `OnceLock`
    /// so the inbox can be installed lazily after construction
    /// (capabilities sometimes have to thread the receiver through
    /// a builder before the transport sees it). `OnceLock::get()`
    /// returns `None` until [`NativeTransport::install_inbox`] runs;
    /// `wait_reply` returns the `ERR_NO_INBOX` sentinel in that
    /// case.
    inbox: OnceLock<Mutex<Receiver<Envelope>>>,
    /// Mismatched envelopes a previous `wait_reply` pulled but
    /// didn't return; consulted before the next `recv_timeout`.
    overflow: Mutex<VecDeque<Envelope>>,
    /// Monotonic correlation counter — atomic so `&self` can mint
    /// new ids without `&mut`.
    correlation: AtomicU64,
}

impl NativeTransport {
    /// Build a fresh transport. Pair `self_mailbox` with the id the
    /// `MailboxClaim` returned (the substrate routes replies back
    /// to it via the `ReplyTarget::Component(self_mailbox)` tag the
    /// transport stamps onto outbound mail). The inbox is installed
    /// separately via [`Self::install_inbox`] so capabilities that
    /// build the transport before pulling the receiver out of their
    /// claim aren't forced into a specific construction order.
    pub fn new(mailer: Arc<Mailer>, self_mailbox: MailboxId) -> Self {
        Self {
            mailer,
            self_mailbox,
            inbox: OnceLock::new(),
            overflow: Mutex::new(VecDeque::new()),
            correlation: AtomicU64::new(0),
        }
    }

    /// Install the receiver half of the actor's inbox so
    /// `wait_reply` has somewhere to pull from. Called once per
    /// transport, before any `wait_reply` invocation. Subsequent
    /// calls panic — the slot is single-claim by construction.
    pub fn install_inbox(&self, inbox: Receiver<Envelope>) {
        self.inbox
            .set(Mutex::new(inbox))
            .unwrap_or_else(|_| panic!("NativeTransport::install_inbox called twice"));
    }

    /// The mailbox id the substrate routes inbound mail through to
    /// reach this actor. Exposed for capabilities that need to
    /// publish their address to peers without going through the
    /// transport's send path.
    pub fn self_mailbox(&self) -> MailboxId {
        self.self_mailbox
    }

    /// Block until the next envelope arrives on this actor's inbox.
    /// Returns `None` when the channel disconnects (the channel-drop
    /// shutdown signal — capability's `RunningCapability::shutdown`
    /// dropped its [`crate::capability::SinkSender`], the registry
    /// handler can no longer upgrade its [`std::sync::Weak`], the
    /// inbox's last sender is gone) or when no inbox is installed.
    ///
    /// The natural shape for a dispatcher loop:
    ///
    /// ```ignore
    /// while let Some(env) = transport.recv_blocking() {
    ///     handle_envelope(env);
    /// }
    /// ```
    ///
    /// Distinct from [`MailTransport::wait_reply`], which filters by
    /// `(kind, correlation)` and returns when a *specific* reply
    /// arrives — `recv_blocking` is for the dispatcher's "next
    /// thing, whatever it is" main loop.
    pub fn recv_blocking(&self) -> Option<Envelope> {
        let inbox = self.inbox.get()?;
        // The mutex guard stays held across `recv()`. Dispatcher
        // threads are single-tasked while parked here; nothing else
        // on this thread contends.
        inbox.lock().unwrap().recv().ok()
    }

    /// Non-blocking variant of [`Self::recv_blocking`]. Returns
    /// `None` for "no envelope available right now" or "channel
    /// disconnected" or "no inbox installed". A capability that
    /// needs to distinguish drains via repeated calls until `None`.
    pub fn try_recv(&self) -> Option<Envelope> {
        let inbox = self.inbox.get()?;
        inbox.lock().unwrap().try_recv().ok()
    }
}

/// Return code surfaced from `send_mail` / `reply_mail` /
/// `save_state` for a no-op or unsupported call. Distinct from
/// substrate-rejected `1` so callers can tell "not implemented" from
/// "rejected by recipient lookup".
const ERR_NOT_IMPLEMENTED: u32 = 0xFFFF_FF00;

/// Negative sentinel for `wait_reply` when no inbox is installed.
/// Picked outside the documented `-1`/`-2`/`-3` range so the SDK's
/// `decode_wait_reply` falls into the unknown-rc branch and surfaces
/// "no inbox installed" by name in the error.
const ERR_NO_INBOX_I32: i32 = 100;

impl MailTransport for NativeTransport {
    fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
        let correlation = self.correlation.fetch_add(1, Ordering::AcqRel) + 1;
        let reply_to =
            ReplyTo::with_correlation(ReplyTarget::Component(self.self_mailbox), correlation);
        let mail = Mail::new(MailboxId(recipient), KindId(kind), bytes.to_vec(), count)
            .with_reply_to(reply_to)
            .with_origin(self.self_mailbox);
        self.mailer.push(mail);
        0
    }

    fn reply_mail(&self, _sender: u32, _kind: u64, _bytes: &[u8], _count: u32) -> u32 {
        // ADR-0074 Phase 2b: the first capability that needs reply
        // is handle (round-trips `Handle{Publish,Release,Pin,Unpin}Result`
        // back to the sender). Until then, the bare-bytes →
        // `Mailer::send_reply` bridge isn't worth designing. Log
        // doesn't reply.
        tracing::error!(
            target: "aether_substrate::native_transport",
            "NativeTransport::reply_mail not yet implemented — Phase 2b lands this when handle migrates"
        );
        ERR_NOT_IMPLEMENTED
    }

    fn save_state(&self, _version: u32, _bytes: &[u8]) -> u32 {
        // Native actors don't have a `replace_component`-style hot
        // reload path (only wasm components do, ADR-0016). The trait
        // method is part of the unified SDK signature; the native
        // impl returns an error sentinel so a misuse is loud.
        tracing::error!(
            target: "aether_substrate::native_transport",
            "NativeTransport::save_state called — native actors don't migrate"
        );
        ERR_NOT_IMPLEMENTED
    }

    fn wait_reply(
        &self,
        expected_kind: u64,
        out: &mut [u8],
        timeout_ms: u32,
        expected_correlation: u64,
    ) -> i32 {
        let Some(inbox_mutex) = self.inbox.get() else {
            tracing::error!(
                target: "aether_substrate::native_transport",
                "wait_reply called without an installed inbox — install_inbox must run first"
            );
            return -ERR_NO_INBOX_I32;
        };

        let timeout = Duration::from_millis(timeout_ms as u64);
        let deadline = Instant::now() + timeout;

        loop {
            // Drain overflow first — a previous `wait_reply` may
            // have parked envelopes that match this kind /
            // correlation. Mirrors `SubstrateCtx::wait_reply` on
            // the wasm side (component.rs).
            let from_overflow = {
                let mut overflow = self.overflow.lock().unwrap();
                let pos = overflow
                    .iter()
                    .position(|env| matches_filter(env, expected_kind, expected_correlation));
                pos.and_then(|i| overflow.remove(i))
            };
            if let Some(env) = from_overflow {
                return write_payload(&env, out);
            }

            // No overflow match — pull from the inbox with whatever
            // time is left on the deadline. The mutex guard stays
            // held across `recv_timeout`; the dispatcher thread is
            // single-tasked while parked here, so no other code on
            // this thread contends with the lock.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let recv_outcome = inbox_mutex.lock().unwrap().recv_timeout(remaining);

            match recv_outcome {
                Ok(env) => {
                    if matches_filter(&env, expected_kind, expected_correlation) {
                        return write_payload(&env, out);
                    }
                    self.overflow.lock().unwrap().push_back(env);
                    // Loop continues — try again with whatever time
                    // is left on the deadline.
                }
                Err(RecvTimeoutError::Timeout) => return -1,
                Err(RecvTimeoutError::Disconnected) => return -3,
            }
        }
    }

    fn prev_correlation(&self) -> u64 {
        self.correlation.load(Ordering::Acquire)
    }
}

fn matches_filter(env: &Envelope, expected_kind: u64, expected_correlation: u64) -> bool {
    env.kind.0 == expected_kind
        && (expected_correlation == ReplyTo::NO_CORRELATION
            || env.sender.correlation_id == expected_correlation)
}

/// Copy `env.payload` into `out` and return the number of bytes
/// written, matching the wasm `wait_reply_p32` ABI:
/// `>= 0` = bytes written, `-2` = payload too large for the buffer
/// (envelope is dropped — wasm re-parks but native callers should
/// retry with a bigger buffer).
fn write_payload(env: &Envelope, out: &mut [u8]) -> i32 {
    if env.payload.len() > out.len() {
        tracing::warn!(
            target: "aether_substrate::native_transport",
            payload_len = env.payload.len(),
            buffer_len = out.len(),
            "wait_reply buffer too small — envelope dropped"
        );
        return -2;
    }
    out[..env.payload.len()].copy_from_slice(&env.payload);
    env.payload.len() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;
    use crate::scheduler::ComponentTable;
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::mpsc;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new());
        let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        mailer.wire(Arc::clone(&registry), components);
        (registry, mailer)
    }

    /// `prev_correlation` returns 0 before any send and tracks the
    /// monotonic counter as `send_mail` mints new ids.
    #[test]
    fn prev_correlation_tracks_send_mail_minting() {
        let (registry, mailer) = fresh_substrate();
        let (tx, _rx) = mpsc::channel::<Envelope>();
        // Register a sink so push routes somewhere instead of
        // hitting the unknown-recipient warn.
        registry.register_sink(
            "test.sink",
            Arc::new(move |kind, kind_name, origin, sender, payload, count| {
                let _ = tx.send(Envelope {
                    kind,
                    kind_name: kind_name.to_owned(),
                    origin: origin.map(str::to_owned),
                    sender,
                    payload: payload.to_vec(),
                    count,
                });
            }),
        );
        let recipient = registry.lookup("test.sink").unwrap();

        let transport = NativeTransport::new(mailer, MailboxId(99));

        assert_eq!(transport.prev_correlation(), 0);
        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        assert_eq!(transport.prev_correlation(), 1);
        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        assert_eq!(transport.prev_correlation(), 2);
    }

    /// `wait_reply` with no inbox installed returns the no-inbox
    /// negative sentinel.
    #[test]
    fn wait_reply_without_inbox_returns_no_inbox_sentinel() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeTransport::new(mailer, MailboxId(1));
        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(0, &mut buf, 1, 0);
        assert_eq!(rc, -ERR_NO_INBOX_I32);
    }

    /// `reply_mail` and `save_state` are tracked stubs — Phase 2a
    /// pins their behaviour so a future implementation is a
    /// straight diff against the test.
    #[test]
    fn reply_mail_and_save_state_are_tracked_stubs() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeTransport::new(mailer, MailboxId(1));
        assert_eq!(transport.reply_mail(0, 0, &[], 0), ERR_NOT_IMPLEMENTED);
        assert_eq!(transport.save_state(0, &[]), ERR_NOT_IMPLEMENTED);
    }

    /// `install_inbox` is single-claim — a second install panics.
    #[test]
    #[should_panic(expected = "install_inbox called twice")]
    fn install_inbox_twice_panics() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeTransport::new(mailer, MailboxId(1));
        let (_tx1, rx1) = mpsc::channel::<Envelope>();
        let (_tx2, rx2) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx1);
        transport.install_inbox(rx2);
    }

    /// `wait_reply` returns the `-1` timeout sentinel when no
    /// envelope arrives within the deadline.
    #[test]
    fn wait_reply_times_out_when_inbox_quiet() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeTransport::new(mailer, MailboxId(1));
        let (_tx, rx) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx);
        let mut buf = [0u8; 16];
        // 1ms is enough — no sender ever pushes.
        let rc = transport.wait_reply(0, &mut buf, 1, 0);
        assert_eq!(rc, -1);
    }
}
