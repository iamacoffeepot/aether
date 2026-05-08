//! `aether.tcp` cap (issue 607 Phase 6a, ADR-0079).
//!
//! Three-tier shape: [`TcpCapability`] (Singleton control plane) →
//! [`TcpListenerActor`] (Instanced, one per bound port) → eventually
//! `TcpSessionActor` (Instanced, Phase 6b — per connection). Phase 6a
//! lands the singleton + listener and a stub accept handler that
//! drops accepted streams; Phase 6b adds the session spawn and the
//! read/write surface.
//!
//! ## Supervision shape
//!
//! `TcpCapability` is the supervisor of its listener fleet: it spawns
//! listeners, monitors them, and replies to unbind requests on their
//! close. The cap holds its own `MailboxId → ListenerEntry` map; it
//! does NOT walk the chassis-wide actor registry to enumerate
//! children. Cap handlers don't introspect the registry — the
//! cap-as-supervisor pattern keeps the actor model intact (caps
//! communicate via mail at runtime; chassis-level introspection is a
//! test/embedder affordance, not a handler-side surface).
//!
//! ## Mail surface
//!
//! Control plane (mailed to `aether.tcp`):
//! - `BindListener { addr, name? }` → `BindListenerResult`
//! - `UnbindListener { listener_name }` → `UnbindListenerResult`
//!   (asynchronous reply: the cap monitors the listener at spawn time
//!   and replies only after `MonitorNotice` arrives)
//! - `ListListeners` → `ListListenersResult`
//!
//! Listener (mailed to `aether.tcp.listener:<name>`):
//! - `Close` → cooperative shutdown via `ctx.shutdown()`
//!
//! ## Threading
//!
//! Each listener owns one sidecar OS thread that holds the
//! `std::net::TcpListener` and runs a blocking accept loop. On
//! `on_close` the listener flips a shutdown flag and self-connects
//! to its bound port to wake the blocked accept; the accept returns,
//! sees the flag, breaks; the dispatcher thread joins.

mod listener;
mod session;

pub use listener::TcpListenerActor;
// TcpListenerConfig and TcpSessionActor / TcpSessionConfig live
// inside the native bridge mod (gated non-wasm32) and are only
// consumed by the cap's spawn path on native — re-exporting
// unconditionally would fail to resolve on wasm32 builds where
// the bridge emits a stub.
#[cfg(not(target_arch = "wasm32"))]
pub use listener::TcpListenerConfig;
#[cfg(not(target_arch = "wasm32"))]
pub use session::{TcpSessionActor, TcpSessionConfig};

// Trait-marker kinds the wasm32 bridge stub references via HandlesKind.
use aether_kinds::{BindListener, ListListeners, MonitorNotice, UnbindListener};
// Reply / payload kinds only consumed by native handler bodies. Gated to
// avoid an unused-import warning on wasm32 where the bridge stub doesn't
// reference them.
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::{
    BindListenerResult, Close, ListListenersResult, ListenerInfo, UnbindListenerResult,
};

#[aether_actor::bridge(singleton)]
mod cap_native {
    // Trait-marker kinds the wasm32 bridge stub needs to satisfy
    // HandlesKind; these stay always-imported.
    use super::{BindListener, ListListeners, MonitorNotice, UnbindListener};
    // Reply / payload kinds + native-only types (TcpListenerActor /
    // TcpListenerConfig) only consumed by native handler bodies. The
    // wasm32 stub bridge emits doesn't reference them, so they're
    // gated to avoid an unused-import warning.
    #[cfg(not(target_arch = "wasm32"))]
    use super::{
        BindListenerResult, Close, ListListenersResult, ListenerInfo, TcpListenerActor,
        TcpListenerConfig, UnbindListenerResult,
    };
    use aether_actor::{MailCtx, actor};
    use aether_substrate::capability::BootError;
    use aether_substrate::native_actor::{MonitorHandle, NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::spawn::Subname;
    use std::collections::HashMap;
    use std::net::TcpListener;

    /// Singleton control-plane cap. Owns the listener fleet directly
    /// — the cap is the supervisor, not a thin shim over the chassis
    /// registry. Each spawn registers a monitor on the new listener
    /// and inserts a [`ListenerEntry`] into the cap-local map; the
    /// `on_monitor_notice` handler removes the entry on listener
    /// close.
    ///
    /// Issue 629 / Phase B: plain `HashMap` fields. The dispatcher
    /// thread is the sole writer / reader; pre-Phase-A's
    /// `Mutex<HashMap<...>>` was a worker-pool-era tax, not a
    /// contention point.
    pub struct TcpCapability {
        /// Live listeners spawned by this cap. Key is the listener's
        /// full-name `MailboxId`. Each entry holds the bind metadata
        /// surfaced via `ListListeners` plus the monitor handle that
        /// pins the cap's monitor on the listener until close.
        listeners: HashMap<aether_data::MailboxId, ListenerEntry>,
        /// Outstanding unbind replies parked until `MonitorNotice`
        /// arrives from the listener being closed. Key is the same
        /// `MailboxId` as `listeners`; the cap's monitor (registered
        /// at spawn time) is what fires the notice.
        pending_unbinds: HashMap<aether_data::MailboxId, PendingUnbind>,
    }

    /// Cap-local supervisor state for one live listener. Drops with
    /// the entry; `MonitorHandle::Drop` is idempotent with the close
    /// path's index drain.
    struct ListenerEntry {
        addr: String,
        port: u16,
        name: String,
        // Held to keep the cap's monitor registered against the
        // listener for its lifetime. Drops when the entry is removed
        // (in `on_monitor_notice`).
        _monitor_handle: MonitorHandle,
    }

    struct PendingUnbind {
        sender: aether_data::ReplyTo,
        listener_name: String,
    }

    #[actor]
    impl NativeActor for TcpCapability {
        type Config = ();
        const NAMESPACE: &'static str = "aether.tcp";

        fn init(_: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                listeners: HashMap::new(),
                pending_unbinds: HashMap::new(),
            })
        }

        /// Spawn a fresh `TcpListenerActor` bound to `mail.addr`.
        ///
        /// Binds the socket on the dispatcher thread (so a bind
        /// failure replies `Err` synchronously), then hands the bound
        /// listener through `spawn_child`. After spawn the cap
        /// registers a monitor and inserts the listener into its
        /// supervisor map.
        ///
        /// # Agent
        /// Reply: `BindListenerResult`. `Ok` on successful bind +
        /// spawn; `Err` on addr parse / bind / spawn / monitor failure.
        #[handler]
        fn on_bind(&mut self, ctx: &mut NativeCtx<'_>, mail: BindListener) {
            let listener = match TcpListener::bind(&mail.addr) {
                Ok(l) => l,
                Err(e) => {
                    ctx.reply(&BindListenerResult::Err {
                        addr: mail.addr,
                        reason: format!("bind failed: {e}"),
                    });
                    return;
                }
            };
            let local_port = match listener.local_addr() {
                Ok(addr) => addr.port(),
                Err(e) => {
                    ctx.reply(&BindListenerResult::Err {
                        addr: mail.addr,
                        reason: format!("local_addr failed: {e}"),
                    });
                    return;
                }
            };
            let subname_str = mail.name.clone().unwrap_or_else(|| format!("{local_port}"));

            let listener_id = match ctx
                .spawn_child::<TcpListenerActor>(
                    Subname::Named(&subname_str),
                    TcpListenerConfig {
                        listener: Some(listener),
                        addr: mail.addr.clone(),
                        port: local_port,
                    },
                )
                .finish()
            {
                Ok(id) => id,
                Err(e) => {
                    ctx.reply(&BindListenerResult::Err {
                        addr: mail.addr,
                        reason: format!("spawn failed: {e:?}"),
                    });
                    return;
                }
            };

            // Register the cap's monitor on the freshly-spawned
            // listener. The monitor pins until the entry is removed
            // (in on_monitor_notice).
            let monitor_handle = match ctx.monitor(listener_id) {
                Ok(h) => h,
                Err(e) => {
                    // Listener spawned but monitor failed — extremely
                    // unlikely (listener was just inserted Live). Reply
                    // Err and let the listener live; chassis shutdown
                    // will reap it.
                    ctx.reply(&BindListenerResult::Err {
                        addr: mail.addr,
                        reason: format!("monitor failed: {e:?}"),
                    });
                    return;
                }
            };

            self.listeners.insert(
                listener_id,
                ListenerEntry {
                    addr: mail.addr.clone(),
                    port: local_port,
                    name: subname_str.clone(),
                    _monitor_handle: monitor_handle,
                },
            );

            ctx.reply(&BindListenerResult::Ok {
                listener_name: subname_str,
                listener_id,
                local_port,
            });
        }

        /// Mail `Close` to the named listener and park the
        /// originator's reply target. Reply fires from
        /// `on_monitor_notice` once the listener tombstones.
        ///
        /// # Agent
        /// Reply: `UnbindListenerResult`. Asynchronous — the response
        /// fires after the listener's accept thread joins and its
        /// `MonitorNotice` arrives at this cap.
        #[handler]
        fn on_unbind(&mut self, ctx: &mut NativeCtx<'_>, mail: UnbindListener) {
            // Resolve listener_id from the cap-local supervisor map by
            // name. The cap is the source of truth for "what listeners
            // exist"; no registry walk needed.
            let listener_id = self
                .listeners
                .iter()
                .find(|(_, entry)| entry.name == mail.listener_name)
                .map(|(id, _)| *id);
            let Some(listener_id) = listener_id else {
                ctx.reply(&UnbindListenerResult::Err {
                    listener_name: mail.listener_name,
                    reason: "no such listener (or already closed)".into(),
                });
                return;
            };
            // Park the reply target keyed on listener_id. The cap's
            // already-registered monitor (set at spawn time) fires
            // MonitorNotice on close, which drives the reply.
            self.pending_unbinds.insert(
                listener_id,
                PendingUnbind {
                    sender: ctx.reply_target(),
                    listener_name: mail.listener_name.clone(),
                },
            );
            // Mail Close to the listener via the SDK typed-send
            // shortcut. The listener's `on_close_request` handler
            // calls `ctx.shutdown()`.
            let full_name = format!(
                "{}:{}",
                <TcpListenerActor as aether_actor::Actor>::NAMESPACE,
                mail.listener_name,
            );
            ctx.resolve_actor::<TcpListenerActor>(&full_name)
                .send(&Close::default());
        }

        /// Walk the cap-local listener map and report metadata.
        ///
        /// # Agent
        /// Reply: `ListListenersResult`.
        #[handler]
        fn on_list(&mut self, ctx: &mut NativeCtx<'_>, _mail: ListListeners) {
            let listeners: Vec<ListenerInfo> = self
                .listeners
                .values()
                .map(|entry| ListenerInfo {
                    name: entry.name.clone(),
                    addr: entry.addr.clone(),
                    port: entry.port,
                })
                .collect();
            ctx.reply(&ListListenersResult { listeners });
        }

        /// Listener tombstoned — remove from the supervisor map and
        /// fire the parked unbind reply if one is waiting.
        ///
        /// `MonitorNotice.target` identifies which listener closed.
        /// The cap's monitor on every spawned listener (registered in
        /// `on_bind`) fires this notice; if the close came from an
        /// unbind request, `pending_unbinds` has an entry with the
        /// originator to reply to.
        #[handler]
        fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
            // Drop the supervisor entry. The held MonitorHandle drops
            // here; deregister is idempotent with the close path's
            // forward-index drain.
            let _entry = self.listeners.remove(&notice.target);
            // Fire the parked unbind reply if one was waiting.
            let parked = self.pending_unbinds.remove(&notice.target);
            if let Some(parked) = parked {
                ctx.transport().send_reply_for_handler(
                    parked.sender,
                    &UnbindListenerResult::Ok {
                        listener_name: parked.listener_name,
                    },
                );
            }
            // Else: notice came from a non-unbind close (chassis
            // shutdown, future trap). Nothing to reply to; the
            // supervisor entry is gone, that's the cleanup.
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        use super::{
            BindListener, BindListenerResult, ListListeners, ListListenersResult, TcpCapability,
            UnbindListener, UnbindListenerResult,
        };
        use aether_actor::Actor;
        use aether_data::{Kind, SessionToken, Uuid};
        use aether_substrate::capability::ChassisBuilder;
        use aether_substrate::mail::{ReplyTarget, ReplyTo};
        use aether_substrate::mailer::Mailer;
        use aether_substrate::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::registry::{MailboxEntry, Registry};

        fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>) {
            let registry = Arc::new(Registry::new());
            for d in aether_kinds::descriptors::all() {
                let _ = registry.register_kind_with_descriptor(d);
            }
            let (outbound, rx) = HubOutbound::attached_loopback();
            let mailer = Arc::new(Mailer::new());
            mailer.wire(Arc::clone(&registry));
            mailer.wire_outbound(outbound);
            (registry, mailer, rx)
        }

        fn session_reply() -> ReplyTo {
            ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::from_u128(0xfeed))))
        }

        /// Push a postcard-encoded mail at the cap's mailbox via the
        /// registered sink handler, then wait for the next outbound
        /// reply on `rx` and decode as `R`.
        fn drive_and_decode<K, R>(
            registry: &Arc<Registry>,
            rx: &mpsc::Receiver<EgressEvent>,
            cap_namespace: &str,
            mail: &K,
        ) -> R
        where
            K: Kind + serde::Serialize,
            R: serde::de::DeserializeOwned,
        {
            let id = registry
                .lookup(cap_namespace)
                .expect("cap mailbox registered");
            let MailboxEntry::Closure(handler) = registry.entry(id).expect("cap entry") else {
                panic!("expected mailbox entry");
            };
            let bytes = postcard::to_allocvec(mail).expect("encode");
            handler(K::ID, K::NAME, None, session_reply(), &bytes, 1);

            let deadline = Instant::now() + Duration::from_secs(2);
            let frame = loop {
                if let Ok(f) = rx.try_recv() {
                    break f;
                }
                if Instant::now() >= deadline {
                    panic!("reply did not arrive within deadline for {}", K::NAME);
                }
                thread::sleep(Duration::from_millis(5));
            };
            let payload = match frame {
                EgressEvent::ToSession { payload, .. } | EgressEvent::Broadcast { payload, .. } => {
                    payload
                }
                other => panic!("expected ToSession/Broadcast egress, got {other:?}"),
            };
            postcard::from_bytes(&payload).expect("decode reply")
        }

        /// Issue 607 Phase 6a: bind → list → unbind round-trip on a
        /// loopback port. Asserts the cap-local supervisor map
        /// reflects every step (bound, listed, unbound).
        #[test]
        fn bind_then_list_then_unbind_roundtrip() {
            let (registry, mailer, rx) = fresh_substrate();
            let _chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<TcpCapability>(())
                .build()
                .expect("TcpCapability boots");

            // Bind to port 0 — let the OS pick a free port.
            let bind_reply: BindListenerResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &BindListener {
                    addr: "127.0.0.1:0".into(),
                    name: None,
                },
            );
            let (listener_name, local_port) = match bind_reply {
                BindListenerResult::Ok {
                    listener_name,
                    local_port,
                    ..
                } => (listener_name, local_port),
                BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
            };
            assert_eq!(
                listener_name,
                local_port.to_string(),
                "default subname should be the bound port",
            );
            assert!(local_port > 0, "OS-picked port should be non-zero");

            // List enumerates the one listener.
            let list_reply: ListListenersResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &ListListeners::default(),
            );
            assert_eq!(list_reply.listeners.len(), 1, "exactly one listener");
            let entry = &list_reply.listeners[0];
            assert_eq!(entry.name, listener_name);
            assert_eq!(entry.port, local_port);
            assert_eq!(entry.addr, "127.0.0.1:0");

            // Unbind — asynchronous reply via MonitorNotice.
            let unbind_reply: UnbindListenerResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &UnbindListener {
                    listener_name: listener_name.clone(),
                },
            );
            match unbind_reply {
                UnbindListenerResult::Ok { listener_name: ln } => assert_eq!(ln, listener_name),
                UnbindListenerResult::Err { reason, .. } => panic!("unbind failed: {reason}"),
            }

            // List should now be empty — cap-local supervisor map
            // dropped the entry on MonitorNotice.
            let list_reply: ListListenersResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &ListListeners::default(),
            );
            assert!(
                list_reply.listeners.is_empty(),
                "list should drop the unbound listener",
            );
        }

        /// Binding the same port twice fails the second bind. Uses
        /// the first bind's actually-bound port to drive the second.
        #[test]
        fn bind_port_in_use_returns_err() {
            let (registry, mailer, rx) = fresh_substrate();
            let _chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<TcpCapability>(())
                .build()
                .expect("TcpCapability boots");

            let first: BindListenerResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &BindListener {
                    addr: "127.0.0.1:0".into(),
                    name: Some("first".into()),
                },
            );
            let local_port = match first {
                BindListenerResult::Ok { local_port, .. } => local_port,
                BindListenerResult::Err { reason, .. } => panic!("first bind failed: {reason}"),
            };

            // Second bind on the same port — must fail.
            let second: BindListenerResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &BindListener {
                    addr: format!("127.0.0.1:{local_port}"),
                    name: Some("second".into()),
                },
            );
            match second {
                BindListenerResult::Ok { .. } => panic!("expected port-in-use Err"),
                BindListenerResult::Err { reason, addr } => {
                    assert_eq!(addr, format!("127.0.0.1:{local_port}"));
                    assert!(
                        reason.starts_with("bind failed:"),
                        "expected bind-fail reason, got: {reason}",
                    );
                }
            }
        }

        /// Unbind on an unknown name surfaces an Err with the name
        /// echoed back.
        #[test]
        fn unbind_unknown_listener_errors() {
            let (registry, mailer, rx) = fresh_substrate();
            let _chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<TcpCapability>(())
                .build()
                .expect("TcpCapability boots");

            let reply: UnbindListenerResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &UnbindListener {
                    listener_name: "nope".into(),
                },
            );
            match reply {
                UnbindListenerResult::Err { listener_name, .. } => {
                    assert_eq!(listener_name, "nope");
                }
                UnbindListenerResult::Ok { .. } => panic!("expected Err for unknown listener"),
            }
        }

        /// Issue 607 Phase 6b: connect a real TCP client to a bound
        /// listener, write bytes, observe `SessionData` broadcast on
        /// the egress, then drop the client and observe
        /// `SessionClosed`. Exercises the full pipeline:
        /// listener accept thread → mpsc → ConnectionReady wake →
        /// listener spawns TcpSessionActor → session read thread →
        /// mpsc → SessionDataReady wake → session broadcasts.
        ///
        /// Boots [`crate::BroadcastCapability`] alongside `TcpCapability`
        /// so the broadcast mailbox is registered; sessions broadcast
        /// `SessionData` / `SessionClosed` through it.
        #[test]
        fn session_round_trip_data_then_close() {
            use std::io::Write;
            use std::net::TcpStream;

            let (registry, mailer, rx) = fresh_substrate();
            let _chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<crate::BroadcastCapability>(())
                .with_actor::<TcpCapability>(())
                .build()
                .expect("caps boot");

            // Bind to OS-picked port.
            let bind_reply: BindListenerResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &BindListener {
                    addr: "127.0.0.1:0".into(),
                    name: None,
                },
            );
            let local_port = match bind_reply {
                BindListenerResult::Ok { local_port, .. } => local_port,
                BindListenerResult::Err { reason, .. } => panic!("bind failed: {reason}"),
            };

            // Connect a real client to the listener and write a
            // tagged payload. Drop the client to trigger EOF on the
            // session's read path.
            let payload_text = b"hello session";
            {
                let mut client = TcpStream::connect(format!("127.0.0.1:{local_port}"))
                    .expect("client connects to listener");
                client.write_all(payload_text).expect("client write");
                // Explicit flush + drop. `Drop` on TcpStream calls
                // shutdown which triggers EOF on the server read.
            }

            // Drain egress until we observe both SessionData and
            // SessionClosed broadcasts. The chassis driver thread
            // (the loopback HubOutbound) ships every broadcast over
            // `rx`. We tolerate other egress events (e.g. Tick
            // broadcasts) interleaved.
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut saw_data = false;
            let mut saw_closed = false;
            while Instant::now() < deadline && (!saw_data || !saw_closed) {
                let frame = match rx.try_recv() {
                    Ok(f) => f,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                let (kind_name, payload) = match frame {
                    EgressEvent::Broadcast {
                        kind_name, payload, ..
                    } => (kind_name, payload),
                    EgressEvent::ToSession {
                        kind_name, payload, ..
                    } => (kind_name, payload),
                    _ => continue,
                };
                if kind_name == <aether_kinds::SessionData as Kind>::NAME {
                    let decoded: aether_kinds::SessionData =
                        postcard::from_bytes(&payload).expect("decode SessionData");
                    assert_eq!(decoded.bytes, payload_text);
                    assert!(decoded.peer.starts_with("127.0.0.1:"));
                    assert!(decoded.session_name.starts_with("conn-"));
                    saw_data = true;
                } else if kind_name == <aether_kinds::SessionClosed as Kind>::NAME {
                    let decoded: aether_kinds::SessionClosed =
                        postcard::from_bytes(&payload).expect("decode SessionClosed");
                    assert_eq!(decoded.reason, "eof");
                    saw_closed = true;
                }
            }
            assert!(saw_data, "SessionData broadcast did not arrive");
            assert!(saw_closed, "SessionClosed broadcast did not arrive");
        }

        /// Two concurrent binds on different ports both surface in
        /// `ListListeners`.
        #[test]
        fn list_enumerates_two_concurrent_listeners() {
            let (registry, mailer, rx) = fresh_substrate();
            let _chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<TcpCapability>(())
                .build()
                .expect("TcpCapability boots");

            let _: BindListenerResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &BindListener {
                    addr: "127.0.0.1:0".into(),
                    name: Some("admin".into()),
                },
            );
            let _: BindListenerResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &BindListener {
                    addr: "127.0.0.1:0".into(),
                    name: Some("game".into()),
                },
            );

            let list: ListListenersResult = drive_and_decode(
                &registry,
                &rx,
                TcpCapability::NAMESPACE,
                &ListListeners::default(),
            );
            let mut names: Vec<String> = list.listeners.iter().map(|l| l.name.clone()).collect();
            names.sort();
            assert_eq!(names, vec!["admin".to_string(), "game".to_string()]);
        }
    }
}
