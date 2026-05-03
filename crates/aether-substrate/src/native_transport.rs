//! ADR-0074 §Decision 6: native-side `MailTransport` implementation.
//!
//! [`NativeTransport`] is a ZST that implements
//! [`aether_actor::MailTransport`] by forwarding each call to the
//! per-actor state stored in a thread-local. Native capabilities call
//! [`install`] at the top of their dispatcher thread and [`uninstall`]
//! before the thread exits; in between, code running on that thread
//! can use the same `Sink<K, T>` / `wait_reply` / `Ctx<'_, T>`
//! machinery the wasm guest path uses through `WasmTransport`.
//!
//! Phase 2a wires `LogCapability` onto this. Log doesn't currently
//! send mail or wait on replies, so the migration is exercising the
//! lifecycle (channel-drop + join) and the install/uninstall plumbing
//! more than the transport methods themselves. The other capabilities
//! migrate one PR at a time per the issue 509 plan; `reply_mail` and
//! `save_state` are tracked stubs until the first capability that
//! needs each one (handle for `reply_mail` in Phase 2b; native actors
//! don't migrate, so `save_state` stays a stub indefinitely).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use aether_actor::MailTransport;

use crate::capability::Envelope;
use crate::mail::{KindId, Mail, MailboxId, ReplyTarget, ReplyTo};
use crate::mailer::Mailer;

/// Per-actor state the [`NativeTransport`] methods need access to.
/// Lives in a thread-local owned by the actor's dispatcher thread; the
/// capability builds it during boot and hands it to [`install`].
///
/// Capability-internal: callers don't construct this directly. The
/// fields are crate-private so future extensions (per-actor
/// observability, panic recovery, etc.) don't break consumers.
pub struct ActorContext {
    pub(crate) mailer: Arc<Mailer>,
    pub(crate) self_mailbox: MailboxId,
    pub(crate) inbox: std::sync::mpsc::Receiver<Envelope>,
    pub(crate) overflow: VecDeque<Envelope>,
    pub(crate) correlation: u64,
}

impl ActorContext {
    /// Build a fresh context. `inbox` is the receiver half of the
    /// mpsc channel registered by `claim_mailbox` /
    /// `claim_mailbox_drop_on_shutdown`; `mailer` is
    /// `ChassisCtx::mail_send_handle()`; `self_mailbox` is the id the
    /// claim returned (used as the `Component` reply-target so
    /// replies route back to this actor's inbox via the mailer).
    pub fn new(
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
        inbox: std::sync::mpsc::Receiver<Envelope>,
    ) -> Self {
        Self {
            mailer,
            self_mailbox,
            inbox,
            overflow: VecDeque::new(),
            correlation: 0,
        }
    }
}

thread_local! {
    /// At most one `ActorContext` is installed per thread at any time.
    /// `RefCell` is the right primitive — single-threaded by
    /// construction (a thread-local can't be aliased across threads),
    /// `borrow_mut` is the load-bearing operation for `send_mail` /
    /// `wait_reply`. Reentrancy is structurally impossible: nothing
    /// `NativeTransport` does calls back into another transport
    /// method on the same thread.
    static CURRENT: RefCell<Option<ActorContext>> = const { RefCell::new(None) };
}

/// Install `ctx` as the current actor context for this thread.
/// Panics if a context is already installed — capabilities are
/// expected to call this exactly once per dispatcher thread, at the
/// top of the spawn closure. The matching [`uninstall`] runs at the
/// end (or on `Drop` if the closure panics, see [`InstallGuard`]).
pub fn install(ctx: ActorContext) {
    CURRENT.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.is_some() {
            panic!(
                "NativeTransport::install called twice on the same thread \
                 — capabilities install once per dispatcher thread"
            );
        }
        *slot = Some(ctx);
    });
}

/// Uninstall the current actor context, dropping the inbox receiver
/// and any overflow. No-op if nothing was installed.
pub fn uninstall() {
    CURRENT.with(|c| {
        c.borrow_mut().take();
    });
}

/// Block until the next envelope arrives on the current actor's
/// inbox. Returns `None` when the channel disconnects (the
/// channel-drop shutdown signal — capability's `RunningCapability::
/// shutdown` dropped its [`SinkSender`](crate::capability::SinkSender),
/// the registry handler can no longer upgrade its [`std::sync::Weak`],
/// the inbox's last sender is gone) or when called outside an actor
/// context.
///
/// The borrow is held only for the duration of the `recv()` call —
/// because dispatcher threads are single-tasked while parked, this
/// is safe; the user code that processes the returned envelope runs
/// after the borrow is released, so it can freely call `send_mail` /
/// `wait_reply` etc. without conflicting with the inbox borrow.
pub fn recv_blocking() -> Option<Envelope> {
    CURRENT.with(|c| {
        let slot = c.borrow();
        let ctx = slot.as_ref()?;
        ctx.inbox.recv().ok()
    })
}

/// Non-blocking variant of [`recv_blocking`]. Returns `None` for
/// "no envelope available right now" or "channel disconnected" or
/// "no actor context installed" — a capability that needs to
/// distinguish drains the inbox via repeated calls until `None`.
pub fn try_recv() -> Option<Envelope> {
    CURRENT.with(|c| {
        let slot = c.borrow();
        let ctx = slot.as_ref()?;
        ctx.inbox.try_recv().ok()
    })
}

/// RAII guard so a panicking dispatcher thread still uninstalls its
/// context. Capabilities that prefer the guarded shape do
/// `let _guard = InstallGuard::install(ctx);` at the top of their
/// thread and let the guard drop on scope exit.
pub struct InstallGuard {
    _private: (),
}

impl InstallGuard {
    pub fn install(ctx: ActorContext) -> Self {
        install(ctx);
        Self { _private: () }
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        uninstall();
    }
}

/// ZST `MailTransport` impl for the native-actor path. See module
/// docs for the install/uninstall protocol.
pub struct NativeTransport;

/// Return code surfaced when the actor SDK is called from a thread
/// that hasn't installed an [`ActorContext`]. Surfaces as a non-zero
/// `u32` from `send_mail` / `reply_mail` / `save_state` and as the
/// "decode" branch (an unexpected negative return) from
/// `wait_reply`.
const ERR_NO_CONTEXT: u32 = 0xFFFF_FF00;

impl MailTransport for NativeTransport {
    fn send_mail(recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
        // Pull everything we need out of the thread-local under one
        // borrow, then release before calling `mailer.push` — push
        // resolves recipient and may invoke a sink handler synchronously
        // on this thread. Holding the borrow across that call would
        // deadlock if any sink handler ever reaches into NativeTransport
        // (none do today, but releasing first is the safer shape).
        let pushed = CURRENT.with(|c| {
            let mut slot = c.borrow_mut();
            let ctx = slot.as_mut()?;
            ctx.correlation = ctx.correlation.wrapping_add(1);
            let correlation = ctx.correlation;
            let mailer = Arc::clone(&ctx.mailer);
            let reply_to =
                ReplyTo::with_correlation(ReplyTarget::Component(ctx.self_mailbox), correlation);
            let mail = Mail::new(MailboxId(recipient), KindId(kind), bytes.to_vec(), count)
                .with_reply_to(reply_to)
                .with_origin(ctx.self_mailbox);
            Some((mailer, mail))
        });

        match pushed {
            Some((mailer, mail)) => {
                mailer.push(mail);
                0
            }
            None => {
                tracing::error!(
                    target: "aether_substrate::native_transport",
                    "send_mail called outside actor context — install() never ran"
                );
                ERR_NO_CONTEXT
            }
        }
    }

    fn reply_mail(_sender: u32, _kind: u64, _bytes: &[u8], _count: u32) -> u32 {
        // ADR-0074 Phase 2b: the first capability that needs reply
        // is handle (round-trips `Handle{Publish,Release,Pin,Unpin}Result`
        // back to the sender). Until then, the bare-bytes →
        // `Mailer::send_reply` bridge isn't worth designing. Log
        // doesn't reply.
        tracing::error!(
            target: "aether_substrate::native_transport",
            "NativeTransport::reply_mail not yet implemented — Phase 2b lands this when handle migrates"
        );
        ERR_NO_CONTEXT
    }

    fn save_state(_version: u32, _bytes: &[u8]) -> u32 {
        // Native actors don't have a `replace_component`-style hot
        // reload path (only wasm components do, ADR-0016). The trait
        // method is part of the unified SDK signature; the native
        // impl returns an error sentinel so a misuse is loud.
        tracing::error!(
            target: "aether_substrate::native_transport",
            "NativeTransport::save_state called — native actors don't migrate"
        );
        ERR_NO_CONTEXT
    }

    fn wait_reply(
        expected_kind: u64,
        out: &mut [u8],
        timeout_ms: u32,
        expected_correlation: u64,
    ) -> i32 {
        let timeout = Duration::from_millis(timeout_ms as u64);
        let deadline = Instant::now() + timeout;

        loop {
            // Drain the overflow first — a previous `wait_reply` call
            // may have parked envelopes that match this kind /
            // correlation. Mirrors `SubstrateCtx::wait_reply` on the
            // wasm side (component.rs).
            let from_overflow = CURRENT.with(|c| {
                let mut slot = c.borrow_mut();
                let ctx = slot.as_mut()?;
                let pos = ctx
                    .overflow
                    .iter()
                    .position(|env| matches_filter(env, expected_kind, expected_correlation))?;
                ctx.overflow.remove(pos)
            });
            if let Some(env) = from_overflow {
                return write_payload(&env, out);
            }

            // Block on the inbox for the remaining time. The borrow
            // stays open across `recv_timeout` because the dispatcher
            // thread is single-tasked while parked here — no other
            // code on this thread re-enters NativeTransport. (Sink
            // handlers that the registry invokes run on the SENDER
            // thread, not this one.)
            let recv_outcome = CURRENT.with(|c| {
                let slot = c.borrow();
                let ctx = slot.as_ref()?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                Some(ctx.inbox.recv_timeout(remaining))
            });

            match recv_outcome {
                None => return -ERR_NO_CONTEXT_I32,
                Some(Ok(env)) => {
                    if matches_filter(&env, expected_kind, expected_correlation) {
                        return write_payload(&env, out);
                    }
                    // Mismatch — park into overflow so a future
                    // `wait_reply` for that kind/correlation finds it.
                    CURRENT.with(|c| {
                        if let Some(ctx) = c.borrow_mut().as_mut() {
                            ctx.overflow.push_back(env);
                        }
                    });
                    // Loop continues — try again with whatever time
                    // is left on the deadline.
                }
                Some(Err(RecvTimeoutError::Timeout)) => return -1,
                Some(Err(RecvTimeoutError::Disconnected)) => return -3,
            }
        }
    }

    fn prev_correlation() -> u64 {
        CURRENT.with(|c| c.borrow().as_ref().map(|ctx| ctx.correlation).unwrap_or(0))
    }
}

/// Negative-return form of [`ERR_NO_CONTEXT`] for `wait_reply`. Picked
/// to be distinct from the documented sentinels (`-1`, `-2`, `-3`) so
/// the SDK's `decode_wait_reply` falls into the unknown-rc branch and
/// surfaces "missing actor context" by name in the error message.
const ERR_NO_CONTEXT_I32: i32 = 100;

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
        // ADR-0042 sentinel `-2`. The wasm side re-parks the mail
        // for retry; the native overflow already drained the env in
        // the calling block, so a "too small" reply is unrecoverable
        // without growing the caller's buffer.
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
    use crate::capability::ChassisBuilder;
    use crate::registry::Registry;
    use crate::scheduler::ComponentTable;
    use std::collections::HashMap;
    use std::sync::{RwLock, mpsc};
    use std::thread;

    /// Smoke: `install` panics if called twice without an `uninstall`
    /// in between. Catches a capability that forgot to clean up.
    #[test]
    fn double_install_panics() {
        // Build a minimal context — registry + mailer don't matter,
        // they're never invoked.
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new());
        let _ = mailer; // keep arc alive
        let (tx, rx) = mpsc::channel::<Envelope>();
        drop(tx);
        let ctx = ActorContext::new(Arc::new(Mailer::new()), MailboxId(1), rx);

        // Run on a fresh thread so we don't pollute other tests'
        // thread-local state.
        let result = thread::spawn(move || {
            install(ctx);
            // Second install with a different ctx must panic.
            let (_tx2, rx2) = mpsc::channel::<Envelope>();
            install(ActorContext::new(
                Arc::new(Mailer::new()),
                MailboxId(2),
                rx2,
            ));
        })
        .join();

        assert!(result.is_err(), "double install should panic");
        let _ = registry; // suppress unused warning on Linux
    }

    /// `uninstall` works even if nothing was installed (idempotent
    /// teardown so a partial-boot path can still call it).
    #[test]
    fn uninstall_without_install_is_noop() {
        thread::spawn(|| {
            uninstall();
            uninstall();
        })
        .join()
        .expect("uninstall should not panic");
    }

    /// `InstallGuard` uninstalls on drop — verified by checking that
    /// a second install on the same thread succeeds after the guard
    /// goes out of scope.
    #[test]
    fn install_guard_uninstalls_on_drop() {
        thread::spawn(|| {
            let (_tx, rx) = mpsc::channel::<Envelope>();
            let ctx = ActorContext::new(Arc::new(Mailer::new()), MailboxId(1), rx);
            {
                let _guard = InstallGuard::install(ctx);
                // Inside the scope: a second install would panic.
            }
            // Guard dropped — install should succeed again.
            let (_tx2, rx2) = mpsc::channel::<Envelope>();
            let ctx2 = ActorContext::new(Arc::new(Mailer::new()), MailboxId(2), rx2);
            install(ctx2);
            uninstall();
        })
        .join()
        .expect("guard should release the slot on drop");
    }

    /// `prev_correlation` returns 0 outside an actor context and
    /// monotonically increases as `send_mail` mints correlations.
    #[test]
    fn prev_correlation_tracks_send_mail_minting() {
        thread::spawn(|| {
            // Outside context: 0.
            assert_eq!(NativeTransport::prev_correlation(), 0);

            // Build a chassis-shaped context. Use the builder so the
            // mailer is wired with a real registry + components table
            // (push needs them).
            let registry = Arc::new(Registry::new());
            let mailer = Arc::new(Mailer::new());
            // Mailer needs a ComponentTable wired before push works.
            // Build through ChassisBuilder which doesn't wire it
            // either — but we don't actually push here, just mint
            // correlations. send_mail's push step will fail because
            // the recipient is unknown, but the correlation is minted
            // before push. Use a name we register as a sink so push
            // routes to the sink handler (which drops the message).
            let _ = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer));
            // Wire mailer for push to be safe to call.
            let components: ComponentTable = Arc::new(RwLock::new(HashMap::new()));
            mailer.wire(Arc::clone(&registry), components);

            let (tx, rx) = mpsc::channel::<Envelope>();
            // Register a sink that swallows the env so push doesn't
            // hit the unknown-recipient warning path.
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

            install(ActorContext::new(mailer, MailboxId(99), rx));

            assert_eq!(NativeTransport::prev_correlation(), 0);
            assert_eq!(NativeTransport::send_mail(recipient.0, 1, &[], 1), 0);
            assert_eq!(NativeTransport::prev_correlation(), 1);
            assert_eq!(NativeTransport::send_mail(recipient.0, 1, &[], 1), 0);
            assert_eq!(NativeTransport::prev_correlation(), 2);

            uninstall();
            assert_eq!(NativeTransport::prev_correlation(), 0);
        })
        .join()
        .expect("test thread should not panic");
    }

    /// `send_mail` outside an actor context returns the no-context
    /// sentinel rather than panicking.
    #[test]
    fn send_mail_without_context_returns_no_context_sentinel() {
        thread::spawn(|| {
            let rc = NativeTransport::send_mail(0, 0, &[], 0);
            assert_eq!(rc, ERR_NO_CONTEXT);
        })
        .join()
        .expect("send_mail outside context should not panic");
    }

    /// `wait_reply` outside an actor context returns the no-context
    /// negative sentinel — the SDK surfaces this as a decode-branch
    /// error naming the rc.
    #[test]
    fn wait_reply_without_context_returns_no_context_sentinel() {
        thread::spawn(|| {
            let mut buf = [0u8; 16];
            let rc = NativeTransport::wait_reply(0, &mut buf, 1, 0);
            assert_eq!(rc, -ERR_NO_CONTEXT_I32);
        })
        .join()
        .expect("wait_reply outside context should not panic");
    }

    /// `reply_mail` and `save_state` are tracked stubs — Phase 2a
    /// pins their behaviour so a future implementation can be a
    /// straight diff against the test.
    #[test]
    fn reply_mail_and_save_state_are_tracked_stubs() {
        thread::spawn(|| {
            assert_eq!(NativeTransport::reply_mail(0, 0, &[], 0), ERR_NO_CONTEXT);
            assert_eq!(NativeTransport::save_state(0, &[]), ERR_NO_CONTEXT);
        })
        .join()
        .expect("stub returns should not panic");
    }
}
