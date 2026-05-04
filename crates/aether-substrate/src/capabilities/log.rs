//! ADR-0070 Phase 3: guest-log sink as a native capability.
//! ADR-0074 Phase 2a: first capability migrated onto the unified
//! actor SDK. Channel-drop + join lifecycle (no `Arc<AtomicBool>`
//! polling); owns a [`NativeTransport`] instance the dispatcher
//! thread uses to talk to the rest of the substrate via the same
//! `Sink<K, _>` / `wait_reply` machinery the wasm guest path uses.
//!
//! Counterpart to the `MailSubscriber` in `aether-component`: decodes
//! `aether.log` mail the guest sent to `aether.log` and re-emits
//! the event through the host's existing tracing pipeline so it shows
//! up in `engine_logs` (ADR-0023) and on stderr alongside native-side
//! logs.
//!
//! Bridging via the `log` crate facade (rather than `tracing::event!`)
//! is load-bearing — see [`crate::log_sink`] for the rationale.
//! `tracing::event!` requires a `&'static str` target; the
//! guest-supplied target is dynamic, so we route through `log::log!`
//! and let `tracing-subscriber`'s `tracing-log` integration lift each
//! record back into the tracing pipeline.
//!
//! Chassis-conditional, NOT universal: desktop, headless, and the
//! test bench register the log capability; the hub chassis does
//! not (no shipped hub chassis hosts components today). Each chassis
//! main calls `boot.add_capability(LogCapability::new())?` after
//! [`crate::SubstrateBoot::build`] returns.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::capability::{BootError, Capability, ChassisCtx, SinkSender};
use crate::log_sink;
use crate::native_transport::NativeTransport;

/// Native capability owning the ADR-0060 guest-log sink. Boots a
/// single dispatcher thread that pulls envelopes from the
/// `aether.log` mailbox and routes the bytes through
/// [`log_sink::handle_log_mail`] for postcard-decode + log-facade
/// emit.
///
/// Post-issue-525-Phase-2 the cap is one struct: pre-boot fields are
/// empty (no constructor config), runtime fields are populated by
/// `boot` and dropped via [`Drop`]. Stateless beyond the per-actor
/// transport — the global tracing subscriber (set up by
/// [`crate::log_capture::init`] during the shared boot) is what
/// actually receives the bridged log records.
#[derive(Default)]
pub struct LogCapability {
    thread: Option<JoinHandle<()>>,
    /// Drop-on-shutdown breaks the channel. Held in an `Option` so
    /// the [`Drop`] impl can take it before joining the thread; the
    /// registry's handler can no longer upgrade its
    /// [`std::sync::Weak`] back-reference, the inbox's last sender
    /// is gone, and the dispatcher's `recv_blocking()` returns
    /// `None` on its next iteration.
    sink_sender: Option<SinkSender>,
    /// The actor's transport. The dispatcher thread holds an
    /// `Arc::clone`, so this field exists to keep the same
    /// transport reachable from chassis-side code that wants to
    /// inspect or coordinate with this actor (none today; the
    /// extension point is here without thread-local plumbing).
    _transport: Option<Arc<NativeTransport>>,
}

impl LogCapability {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Capability for LogCapability {
    /// Components mail `aether.log` (kind id) to this mailbox; the
    /// SDK's `MailSubscriber` resolves through here. The
    /// `aether.<name>` form is the post-ADR-0074 Phase 5 convention
    /// for chassis-owned mailboxes.
    const NAMESPACE: &'static str = "aether.log";

    fn boot(mut self, ctx: &mut ChassisCtx<'_>) -> Result<Self, BootError> {
        let claim = ctx.claim_mailbox_drop_on_shutdown::<Self>()?;
        let mailbox_id = claim.id;

        // ADR-0074 §Decision: `&self` trait, owned transport. The
        // capability constructs its `NativeTransport` once at boot
        // and clones the `Arc` into the dispatcher thread; the
        // dispatcher uses `transport.recv_blocking()` to pull from
        // its own inbox without thread-local plumbing. Going through
        // `from_ctx` wires the chassis's frame-bound set + aborter
        // into the transport so the cross-class `wait_reply` guard
        // (ADR-0074 §Decision 5) is live for any future log-side
        // sync calls — `LogCapability::FRAME_BARRIER` is the default
        // `false`, so the guard treats this caller as free-running.
        let transport = Arc::new(NativeTransport::from_ctx(
            ctx,
            mailbox_id,
            Self::FRAME_BARRIER,
        ));
        transport.install_inbox(claim.receiver);
        let dispatcher_transport = Arc::clone(&transport);

        let thread = thread::Builder::new()
            .name("aether-log-sink".into())
            .spawn(move || {
                // Channel-drop + join: pull until the sender side
                // disconnects. Worst-case shutdown latency is the
                // OS scheduler's wakeup, not a 100ms poll interval.
                while let Some(env) = dispatcher_transport.recv_blocking() {
                    log_sink::handle_log_mail(&env.payload);
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        self.thread = Some(thread);
        self.sink_sender = Some(claim.sink_sender);
        self._transport = Some(transport);
        Ok(self)
    }
}

impl Drop for LogCapability {
    fn drop(&mut self) {
        // Drop the strong sender first to break the channel.
        self.sink_sender.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::mailer::Mailer;
    use crate::registry::{MailboxEntry, Registry};
    use aether_data::Kind;
    use aether_kinds::LogEvent;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    /// End-to-end: boot the capability, push an `aether.log` mail at
    /// the registered sink, the dispatcher thread runs
    /// `handle_log_mail` (which on a test runner with no global
    /// tracing subscriber drops the record silently — what we're
    /// asserting is that the dispatch path doesn't panic and that
    /// shutdown joins cleanly).
    ///
    /// Post-ADR-0074: shutdown latency is bounded by `recv()`
    /// returning on channel disconnect, not by a polling interval.
    /// Channel-drop should land well under the 500ms test budget.
    #[test]
    fn capability_routes_log_event_through_dispatcher() {
        let (registry, mailer) = fresh_substrate();
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(LogCapability::new())
            .build()
            .expect("capability boots");

        let id = registry
            .lookup(LogCapability::NAMESPACE)
            .expect("sink registered");
        let MailboxEntry::Sink(handler) = registry.entry(id).expect("entry") else {
            panic!("expected sink entry");
        };

        let event = LogEvent {
            level: 3,
            target: "aether_test_guest".into(),
            message: "parse failed: missing close paren".into(),
        };
        let bytes = postcard::to_allocvec(&event).expect("encode");
        handler(
            <LogEvent as Kind>::ID,
            "aether.log",
            None,
            crate::mail::ReplyTo::NONE,
            &bytes,
            1,
        );

        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        chassis.shutdown();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "shutdown should complete promptly via channel-drop"
        );
    }

    /// Builder rejects a duplicate claim if the well-known sink name
    /// was already registered.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let (registry, mailer) = fresh_substrate();
        registry.register_sink(LogCapability::NAMESPACE, Arc::new(|_, _, _, _, _, _| {}));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(LogCapability::new())
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name } if name == LogCapability::NAMESPACE
        ));
    }
}
