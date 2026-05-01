//! ADR-0070 Phase 2: handle sink as a native capability.
//!
//! First capability extraction. The handle sink (ADR-0045 typed-handle
//! store front) was the lowest-state, lowest-coupling sink — one
//! mailbox, one [`HandleStore`], no chassis-feature gating — so it's
//! the right place to validate the [`Capability`] trait shape end-to-
//! end before extracting the heavier ones (io, net, audio, render).
//!
//! Behavior change vs the pre-Phase-2 sink: the kernel-side dispatch
//! used to run the per-kind handlers synchronously on the calling
//! thread (a component dispatcher under ADR-0038). The capability
//! shape mandated by the trait is one-actor-thread, so handle ops
//! now serialize on the capability's own OS thread instead of
//! running in parallel from N component dispatchers. The store's
//! internal `RwLock` already serialized the contended path, so
//! correctness is preserved; latency adds one mpsc hop per op (sub-
//! microsecond on uncontended channels). See ADR-0070 §"Threading
//! model".
//!
//! Shutdown is signalled via an [`AtomicBool`] the dispatcher polls
//! on `recv_timeout`. Worst-case shutdown latency is the timeout
//! interval (currently 100ms), which is fine for chassis teardown
//! and avoids tying the trait API to a specific channel type.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::capability::{BootError, Capability, ChassisCtx, RunningCapability};
use crate::handle_sink;
use crate::handle_store::HandleStore;

/// Recipient name the handle capability claims. Components mail
/// `aether.handle.{publish,release,pin,unpin}` (kind ids) to this
/// mailbox; the SDK's `Ctx::publish` / `Handle<K>::Drop` pair both
/// resolve through here. ADR-0058 places chassis-owned sinks under
/// `aether.sink.*`.
pub const HANDLE_SINK_NAME: &str = "aether.sink.handle";

/// Polling interval for the dispatcher's shutdown check. See module
/// docs for the latency tradeoff.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Native capability owning the ADR-0045 typed-handle sink. Boots a
/// single dispatcher thread that pulls envelopes from the
/// `aether.sink.handle` mailbox and routes them through
/// [`handle_sink::dispatch`].
pub struct HandleCapability {
    store: Arc<HandleStore>,
}

impl HandleCapability {
    /// Construct against the substrate's `HandleStore`. The store is
    /// shared with `Mailer::wire_handle_store` so dispatch-time
    /// `Ref<Handle>` resolution and capability-handled
    /// publish/release/pin/unpin observe the same entries.
    pub fn new(store: Arc<HandleStore>) -> Self {
        Self { store }
    }
}

/// Running handle returned by [`HandleCapability::boot`]. Holds the
/// dispatcher thread and the shutdown flag the thread polls. Drop
/// alone does not stop the thread — call [`HandleRunning::shutdown`]
/// (or let the parent [`crate::BootedChassis`] do it on drop).
pub struct HandleRunning {
    thread: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl Capability for HandleCapability {
    type Running = HandleRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let claim = ctx.claim_mailbox(HANDLE_SINK_NAME)?;
        let mailer = ctx.mail_send_handle();
        let store = self.store;
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let thread_flag = Arc::clone(&shutdown_flag);
        let receiver = claim.receiver;

        let thread = thread::Builder::new()
            .name("aether-handle-sink".into())
            .spawn(move || {
                while !thread_flag.load(Ordering::Relaxed) {
                    match receiver.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                        Ok(env) => {
                            handle_sink::dispatch(
                                &store,
                                &mailer,
                                env.kind,
                                env.sender,
                                &env.payload,
                            );
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(HandleRunning {
            thread: Some(thread),
            shutdown_flag,
        })
    }
}

impl RunningCapability for HandleRunning {
    fn shutdown(self: Box<Self>) {
        let HandleRunning {
            mut thread,
            shutdown_flag,
        } = *self;
        shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(t) = thread.take() {
            // Join discards the thread's result; a panic on the
            // dispatcher already routed through the panic hook (issue
            // 321), so there's nothing further to surface here.
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::mpsc;
    use std::time::Instant;

    use aether_data::{HandleId, Kind, KindId};
    use aether_hub_protocol::{EngineToHub, SessionToken, Uuid};
    use aether_kinds::{HandlePublish, HandlePublishResult};

    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::hub_client::HubOutbound;
    use crate::mail::{ReplyTarget, ReplyTo};
    use crate::mailer::Mailer;
    use crate::registry::{MailboxEntry, Registry};

    /// Build a minimally-wired kernel for capability tests: registry
    /// with every kind descriptor (so `send_reply` resolves names),
    /// mailer wired with a loopback hub outbound + the store. The
    /// returned receiver carries every reply the capability emits via
    /// `Mailer::send_reply` along the hub-outbound branch.
    fn fresh_kernel() -> (
        Arc<HandleStore>,
        Arc<Mailer>,
        Arc<Registry>,
        mpsc::Receiver<EngineToHub>,
    ) {
        let store = Arc::new(HandleStore::new(64 * 1024));
        let registry = Arc::new(Registry::new());
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, rx) = HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry), Arc::new(RwLock::new(HashMap::new())));
        mailer.wire_outbound(outbound);
        mailer.wire_handle_store(Arc::clone(&store));
        (store, mailer, registry, rx)
    }

    fn session_reply_to() -> ReplyTo {
        ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::from_u128(0xfeed))))
    }

    /// End-to-end: boot the capability, push a `HandlePublish` mail
    /// at the registered sink, the dispatcher thread routes it
    /// through `handle_sink::dispatch`, the reply lands on the
    /// hub-outbound channel.
    #[test]
    fn capability_routes_publish_through_dispatcher_thread() {
        let (store, mailer, registry, rx) = fresh_kernel();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(HandleCapability::new(Arc::clone(&store)))
            .build()
            .expect("capability boots");

        // Resolve the sink the capability registered.
        let id = registry.lookup(HANDLE_SINK_NAME).expect("sink registered");
        let MailboxEntry::Sink(handler) = registry.entry(id).expect("entry") else {
            panic!("expected sink entry");
        };

        let req = HandlePublish {
            kind_id: KindId(0xCAFE),
            bytes: vec![1, 2, 3, 4, 5],
        };
        let bytes = postcard::to_allocvec(&req).unwrap();
        handler(
            <HandlePublish as Kind>::ID,
            "aether.handle.publish",
            None,
            session_reply_to(),
            &bytes,
            1,
        );

        // The dispatcher runs on its own thread, so poll for the
        // reply with a generous deadline.
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            if let Ok(f) = rx.try_recv() {
                break f;
            }
            if Instant::now() >= deadline {
                panic!("publish reply did not arrive within deadline");
            }
            thread::sleep(Duration::from_millis(5));
        };
        let payload = match frame {
            EngineToHub::Mail(m) => m.payload,
            other => panic!("expected Mail frame, got {other:?}"),
        };
        let result: HandlePublishResult = postcard::from_bytes(&payload).unwrap();
        let HandlePublishResult::Ok {
            kind_id,
            id: handle_id,
        } = result
        else {
            panic!("expected Ok, got {result:?}");
        };
        assert_eq!(kind_id, KindId(0xCAFE));
        assert_ne!(handle_id, HandleId(0));
        let (stored_kind, stored_bytes) = store.get(handle_id).unwrap();
        assert_eq!(stored_kind, KindId(0xCAFE));
        assert_eq!(stored_bytes, vec![1, 2, 3, 4, 5]);

        chassis.shutdown();
    }

    /// Shutdown joins the dispatcher thread cleanly. The polling loop
    /// must return within ~SHUTDOWN_POLL_INTERVAL of the flag set.
    #[test]
    fn shutdown_joins_dispatcher_thread() {
        let (store, mailer, registry, _rx) = fresh_kernel();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(HandleCapability::new(Arc::clone(&store)))
            .build()
            .expect("capability boots");

        let start = Instant::now();
        chassis.shutdown();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "shutdown should complete within a poll interval (took {elapsed:?})"
        );
    }

    /// Builder rejects a duplicate claim if the well-known sink name
    /// was already registered. Guards against the side-by-side window
    /// where a phase-2 PR didn't clean up its legacy
    /// `register_sink(HANDLE_SINK_NAME, ...)` call.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let (store, mailer, registry, _rx) = fresh_kernel();
        registry.register_sink(HANDLE_SINK_NAME, Arc::new(|_, _, _, _, _, _| {}));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(HandleCapability::new(Arc::clone(&store)))
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name } if name == HANDLE_SINK_NAME
        ));
    }
}
