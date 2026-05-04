//! ADR-0070 Phase 2: handle sink as a native capability.
//! ADR-0074 Phase 2b: lifecycle migrated onto channel-drop + join,
//! mirroring [`crate::capabilities::log::LogCapability`]. Worst-case
//! shutdown latency is now the OS scheduler's wakeup on
//! `recv()`-disconnect rather than the prior 100ms `recv_timeout`
//! polling interval. The reply path still routes through
//! [`Mailer::send_reply`] directly (the typed `ctx.reply` SDK pattern
//! is queued for a separate refactor that touches every reply-bearing
//! capability — handle, audio, io, net — at once so the
//! [`crate::native_transport::NativeTransport::reply_mail`] stub fires
//! on a real consumer rather than speculatively).
//!
//! Behavior change vs the pre-Phase-2 sink: the substrate-side dispatch
//! used to run the per-kind handlers synchronously on the calling
//! thread (a component dispatcher under ADR-0038). The capability
//! shape mandated by the trait is one-actor-thread, so handle ops
//! now serialize on the capability's own OS thread instead of
//! running in parallel from N component dispatchers. The store's
//! internal `RwLock` already serialized the contended path, so
//! correctness is preserved; latency adds one mpsc hop per op (sub-
//! microsecond on uncontended channels). See ADR-0070 §"Threading
//! model".

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::capability::{BootError, Capability, ChassisCtx, RunningCapability, SinkSender};
use crate::handle_sink;
use crate::handle_store::HandleStore;
use crate::mailer::Mailer;
use crate::native_transport::NativeTransport;

/// Recipient name the handle capability claims. Components mail
/// `aether.handle.{publish,release,pin,unpin}` (kind ids) to this
/// mailbox; the SDK's `Ctx::publish` / `Handle<K>::Drop` pair both
/// resolve through here. ADR-0058 places chassis-owned sinks under
/// `aether.sink.*`.
pub const HANDLE_SINK_NAME: &str = "aether.sink.handle";

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
/// dispatcher's `JoinHandle`, the [`SinkSender`] strong handle that
/// drives channel-drop shutdown, and the actor's
/// [`NativeTransport`] (kept alive for the dispatcher thread's
/// lifetime via the `Arc` clone the spawn closure holds).
pub struct HandleRunning {
    thread: Option<JoinHandle<()>>,
    /// Drop-on-shutdown breaks the channel. Held by name so the
    /// `RunningCapability::shutdown` impl can drop it explicitly
    /// before joining the thread; the registry's handler can no
    /// longer upgrade its [`std::sync::Weak`] back-reference, the
    /// inbox's last sender is gone, and the dispatcher's
    /// `recv_blocking()` returns `None` on its next iteration.
    sink_sender: Option<SinkSender>,
    /// The actor's transport. The dispatcher thread holds an
    /// `Arc::clone`, so this field exists to keep the same transport
    /// reachable from chassis-side code that wants to inspect or
    /// coordinate with this actor (none today; the extension point is
    /// here without thread-local plumbing).
    _transport: Arc<NativeTransport>,
}

impl Capability for HandleCapability {
    type Running = HandleRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let claim = ctx.claim_mailbox_drop_on_shutdown(HANDLE_SINK_NAME)?;
        let mailer: Arc<Mailer> = ctx.mail_send_handle();
        let mailbox_id = claim.id;
        let store = self.store;

        // ADR-0074 §Decision: `&self` trait, owned transport. The
        // capability constructs its `NativeTransport` once at boot
        // and clones the `Arc` into the dispatcher thread; the
        // dispatcher uses `transport.recv_blocking()` to pull from
        // its own inbox without thread-local plumbing.
        let transport = Arc::new(NativeTransport::from_ctx(
            ctx,
            mailbox_id,
            Self::FRAME_BARRIER,
        ));
        transport.install_inbox(claim.receiver);
        let dispatcher_transport = Arc::clone(&transport);

        let thread = thread::Builder::new()
            .name("aether-handle-sink".into())
            .spawn(move || {
                // Channel-drop + join: pull until the sender side
                // disconnects. Worst-case shutdown latency is the
                // OS scheduler's wakeup, not a 100ms poll interval.
                while let Some(env) = dispatcher_transport.recv_blocking() {
                    handle_sink::dispatch(&store, &mailer, env.kind, env.sender, &env.payload);
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(HandleRunning {
            thread: Some(thread),
            sink_sender: Some(claim.sink_sender),
            _transport: transport,
        })
    }
}

impl RunningCapability for HandleRunning {
    fn shutdown(self: Box<Self>) {
        let HandleRunning {
            mut thread,
            mut sink_sender,
            _transport,
        } = *self;
        // Drop the strong sender first to break the channel.
        sink_sender.take();
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
    use std::time::{Duration, Instant};

    use aether_data::{HandleId, Kind, KindId};
    use aether_data::{SessionToken, Uuid};
    use aether_kinds::{HandlePublish, HandlePublishResult};

    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::mail::{ReplyTarget, ReplyTo};
    use crate::mailer::Mailer;
    use crate::outbound::EgressEvent;
    use crate::registry::{MailboxEntry, Registry};

    /// Build a minimally-wired substrate for capability tests: registry
    /// with every kind descriptor (so `send_reply` resolves names),
    /// mailer wired with a recording outbound + the store. The
    /// returned receiver carries every egress the capability emits via
    /// `Mailer::send_reply` along the hub-outbound branch (substrate-
    /// side `EgressEvent` shape).
    fn fresh_substrate() -> (
        Arc<HandleStore>,
        Arc<Mailer>,
        Arc<Registry>,
        mpsc::Receiver<EgressEvent>,
    ) {
        let store = Arc::new(HandleStore::new(64 * 1024));
        let registry = Arc::new(Registry::new());
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, rx) = crate::outbound::HubOutbound::attached_loopback();
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
        let (store, mailer, registry, rx) = fresh_substrate();

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
            EgressEvent::ToSession { payload, .. } | EgressEvent::Broadcast { payload, .. } => {
                payload
            }
            other => panic!("expected ToSession/Broadcast egress, got {other:?}"),
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

    /// Post-ADR-0074: shutdown latency is bounded by `recv()`
    /// returning on channel disconnect, not by a polling interval.
    /// Channel-drop should land well under the 500ms budget.
    #[test]
    fn shutdown_joins_dispatcher_thread() {
        let (store, mailer, registry, _rx) = fresh_substrate();

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(HandleCapability::new(Arc::clone(&store)))
            .build()
            .expect("capability boots");

        let start = Instant::now();
        chassis.shutdown();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "shutdown should complete promptly via channel-drop (took {elapsed:?})"
        );
    }

    /// Builder rejects a duplicate claim if the well-known sink name
    /// was already registered. Guards against the side-by-side window
    /// where a phase-2 PR didn't clean up its legacy
    /// `register_sink(HANDLE_SINK_NAME, ...)` call.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let (store, mailer, registry, _rx) = fresh_substrate();
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
