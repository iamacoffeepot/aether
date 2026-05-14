//! `aether.engine.proxy:<id>` — per-engine proxy actor (issue 763 P3).
//!
//! An instanced [`NativeActor`] that wraps one *outbound* RPC client
//! connection to a substrate. The forward-model architecture (issue
//! 763) makes every substrate an RPC server; the hub is the client.
//! Each substrate the hub talks to gets one proxy, addressed
//! `aether.engine.proxy:<engine-id>`.
//!
//! ## What it does
//!
//! - **`init`** dials the substrate's `RpcServerCapability` via
//!   [`RpcClient::connect`] and spawns the reader sidecar. The
//!   handshake's `HelloAck` identity is kept on `conn.server`.
//! - **`on_forward`** ([`ForwardEnvelope`]) wraps the `mailbox`,
//!   `kind`, and `payload` into an RPC `Call` and writes it down the
//!   connection. The inbound mail's [`ReplyTo`] is parked under the
//!   wire `cid` so the eventual reply can route back to the sender.
//! - **`on_inbound_ready`** ([`RpcInboundReady`]) is the reader
//!   sidecar's wake: it drains `conn.inbound`, lifting `ReplyEvent`
//!   frames back to the parked `ReplyTo` (correlation preserved,
//!   mirroring `Mailer::send_reply`), dropping the `in_flight` entry on
//!   `ReplyEnd`, and self-shutting-down on `Bye`.
//!
//! ## Scope (issue 763 P3 vs P4)
//!
//! P3 is the bridge core: connect, forward, route replies, lifecycle.
//! The engine-management surface — `describe_kinds` / `list` / `spawn`
//! / `terminate` and the hub RPC server's `engine = Some(_)` routing
//! that drives `ForwardEnvelope` at the proxy — lands in P4 with the
//! engines cap. The cached `HelloAck` manifest the describe handler
//! will read is already in hand on `conn.server`.
//!
//! Native-only: the module owns a `TcpStream` (via [`RpcConnection`])
//! and an OS thread. The `#[bridge]` macro emits the wasm-side marker
//! stub so `aether-capabilities` still compiles for `wasm32`.

// Handler-signature kinds must be importable at file root — the
// `#[bridge]` macro emits `impl HandlesKind<K>` markers as siblings of
// the mod.
use aether_kinds::{ForwardEnvelope, RpcInboundReady};

// `EngineProxyConfig` carries only wasm-safe types, but it lives inside
// the bridge mod (which the macro elides on wasm), so the re-export is
// gated like `TcpListenerConfig`.
#[cfg(not(target_arch = "wasm32"))]
pub use proxy_native::EngineProxyConfig;

#[aether_actor::bridge(instanced)]
mod proxy_native {
    use super::{ForwardEnvelope, RpcInboundReady};
    use crate::rpc::{MailEnvelope, MailboxAddress, PeerKind, RpcClient, RpcConnection, WireFrame};
    use aether_actor::actor;
    use aether_data::{EngineId, Kind, KindId};
    use aether_substrate::Mail;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::{ReplyTarget, ReplyTo};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Init config for [`EngineProxy`]. `engine_id` is the proxy's
    /// engine identity (also the per-instance subname — full address
    /// `aether.engine.proxy:<engine_id>`); `rpc_addr` is the
    /// substrate's `RpcServerCapability` bind address the proxy dials
    /// at init.
    pub struct EngineProxyConfig {
        pub engine_id: EngineId,
        pub rpc_addr: String,
    }

    /// Per-engine proxy: one outbound RPC connection to one substrate,
    /// plus the in-flight reply-correlation table.
    pub struct EngineProxy {
        engine_id: EngineId,
        /// Cached so `on_inbound_ready` can push correlation-preserving
        /// reply mail — `NativeCtx` doesn't expose `mailer()`, only
        /// `NativeInitCtx` does.
        mailer: Arc<Mailer>,
        /// The live outbound connection: `.client` writes `Call`s,
        /// `.inbound` carries reply frames, `.reader` joins on drop.
        /// `.server` holds the substrate's `HelloAck` identity (the
        /// kind manifest P4's describe handler will read).
        conn: RpcConnection,
        /// wire `cid` → the `ReplyTo` of the `ForwardEnvelope` that
        /// opened the call. `ReplyEvent` frames route back here;
        /// `ReplyEnd` clears the entry.
        in_flight: HashMap<u64, ReplyTo>,
    }

    #[actor]
    impl NativeActor for EngineProxy {
        type Config = EngineProxyConfig;
        const NAMESPACE: &'static str = "aether.engine.proxy";

        fn init(config: EngineProxyConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let self_mailbox = ctx.self_id();
            let mailer = ctx.mailer();

            // The reader sidecar fires `RpcInboundReady` at this
            // proxy's own mailbox after every inbound frame so
            // `on_inbound_ready` drains `conn.inbound` on the
            // dispatcher thread.
            let wake_mailer = Arc::clone(&mailer);
            let wake_kind = KindId(<RpcInboundReady as Kind>::ID.0);
            let on_frame = move || {
                wake_mailer.push(Mail::new(self_mailbox, wake_kind, Vec::new(), 1));
            };

            let conn = RpcClient::connect(
                &config.rpc_addr,
                PeerKind::Client {
                    client_name: "aether.engine.proxy".to_owned(),
                    client_version: env!("CARGO_PKG_VERSION").to_owned(),
                },
                on_frame,
            )
            .map_err(|e| BootError::Other(Box::new(e)))?;

            tracing::info!(
                target: "aether_substrate::engine_proxy",
                engine_id = ?config.engine_id,
                addr = %config.rpc_addr,
                "engine proxy connected",
            );

            Ok(Self {
                engine_id: config.engine_id,
                mailer,
                conn,
                in_flight: HashMap::new(),
            })
        }

        /// Relay one mail to the substrate as an RPC `Call`.
        ///
        /// # Agent
        /// Hand the proxy a `ForwardEnvelope { mailbox, kind, payload }`
        /// — the `mailbox` is the *substrate-local* recipient, `kind` +
        /// `payload` the mail to deliver there. Any reply routes back to
        /// the sender of this `ForwardEnvelope`.
        #[handler]
        fn on_forward(&mut self, ctx: &mut NativeCtx<'_>, mail: ForwardEnvelope) {
            let envelope = MailEnvelope {
                to: MailboxAddress::local(mail.mailbox),
                from: None,
                kind: mail.kind,
                correlation_id: None,
                payload: mail.payload,
            };
            match self.conn.client.call(envelope) {
                Ok(cid) => {
                    self.in_flight.insert(cid, ctx.reply_target());
                }
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::engine_proxy",
                        engine_id = ?self.engine_id,
                        error = %e,
                        "engine proxy: Call write failed; dropping forward",
                    );
                }
            }
        }

        /// Reader-sidecar wake. Drain every inbound frame.
        ///
        /// # Agent
        /// Internal wake mail — not part of the proxy's external
        /// surface. The reader thread fires this after pushing a frame;
        /// the handler drains `conn.inbound` and routes each frame.
        #[handler]
        fn on_inbound_ready(&mut self, ctx: &mut NativeCtx<'_>, _mail: RpcInboundReady) {
            while let Ok(frame) = self.conn.inbound.try_recv() {
                match frame {
                    WireFrame::ReplyEvent { cid, envelope } => self.route_reply(cid, envelope),
                    WireFrame::ReplyEnd { cid, .. } => {
                        self.in_flight.remove(&cid);
                    }
                    WireFrame::Bye { reason } => {
                        tracing::info!(
                            target: "aether_substrate::engine_proxy",
                            engine_id = ?self.engine_id,
                            reason = %reason,
                            "engine proxy: substrate closed the connection; shutting down",
                        );
                        ctx.shutdown();
                        return;
                    }
                    // Hello / HelloAck / Call / Ping / Pong: a client-
                    // side proxy never expects these inbound. Drop with
                    // a debug line rather than warn-storming.
                    other => {
                        tracing::debug!(
                            target: "aether_substrate::engine_proxy",
                            engine_id = ?self.engine_id,
                            frame = ?other,
                            "engine proxy: unexpected inbound frame; ignoring",
                        );
                    }
                }
            }
        }
    }

    impl EngineProxy {
        /// Route a `ReplyEvent`'s envelope back to whoever sent the
        /// `ForwardEnvelope` that opened `cid`. Mirrors
        /// `Mailer::send_reply`'s `Component` branch: push a `Mail`
        /// carrying the reply kind + already-encoded bytes, with the
        /// original `correlation_id` echoed (reply-to `None` — nobody
        /// replies to a reply) so a correlation-matching caller picks
        /// it up.
        fn route_reply(&mut self, cid: u64, envelope: MailEnvelope) {
            let Some(reply_to) = self.in_flight.get(&cid).copied() else {
                tracing::debug!(
                    target: "aether_substrate::engine_proxy",
                    engine_id = ?self.engine_id,
                    cid,
                    "engine proxy: ReplyEvent with no matching in-flight forward; dropping",
                );
                return;
            };
            let ReplyTarget::Component(target) = reply_to.target else {
                // The `ForwardEnvelope` arrived with no `Component`
                // reply target (broadcast / `None`) — there's nowhere
                // local to route the reply.
                return;
            };
            self.mailer.push(
                Mail::new(target, envelope.kind, envelope.payload, 1).with_reply_to(
                    ReplyTo::with_correlation(ReplyTarget::None, reply_to.correlation_id),
                ),
            );
        }
    }
}

#[cfg(test)]
use crate::rpc::test_echo::TestEchoReply;

/// Test-only sink: records the `value` of every [`TestEchoReply`] it
/// receives into a shared cell so the round-trip test can observe a
/// reply routed back through the proxy. Lives at file root (not nested
/// in `mod tests`) so the `#[bridge]` macro's marker emission stays
/// addressable.
#[cfg(test)]
#[aether_actor::bridge(singleton)]
mod proxy_reply_sink {
    use super::TestEchoReply;
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use std::sync::{Arc, Mutex};

    pub struct ProxyReplySink {
        recorded: Arc<Mutex<Option<u64>>>,
    }

    #[actor]
    impl NativeActor for ProxyReplySink {
        type Config = Arc<Mutex<Option<u64>>>;
        const NAMESPACE: &'static str = "aether.engine.test.reply_sink";

        fn init(
            recorded: Arc<Mutex<Option<u64>>>,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { recorded })
        }

        #[handler]
        fn on_reply(&mut self, _ctx: &mut NativeCtx<'_>, reply: TestEchoReply) {
            *self.recorded.lock().unwrap() = Some(reply.value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EngineProxy, EngineProxyConfig, ProxyReplySink};
    use crate::rpc::server::{RpcServerCapability, RpcServerConfig, RpcServerHandle};
    use crate::rpc::test_echo::{TestEchoActor, TestEchoRequest};
    use crate::rpc::wire::PeerKind;
    use crate::trace::TraceObserverCapability;
    use aether_actor::Actor;
    use aether_data::{EngineId, Kind, Uuid, mailbox_id_from_name};
    use aether_substrate::Subname;
    use aether_substrate::chassis::Chassis;
    use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::handle_store::HandleStore;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::mail::{Mail, ReplyTarget, ReplyTo};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct TestChassis;
    impl Chassis for TestChassis {
        const PROFILE: &'static str = "test";
        type Driver = NeverDriver;
        type Env = ();
        fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
            unreachable!("TestChassis is driven by Builder::new directly in unit tests")
        }
    }

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
        (registry, mailer)
    }

    fn substrate_peer_kind() -> PeerKind {
        PeerKind::Substrate {
            engine_name: "test".into(),
            engine_version: "0.1.0".into(),
            kinds: vec![],
        }
    }

    /// Full bridge round-trip: boot an RPC server + the echo actor + a
    /// reply sink, spawn an `EngineProxy` pointed at the server's port,
    /// forge a `ForwardEnvelope` at the proxy with the sink as
    /// reply-to, and observe the echoed value land on the sink — proof
    /// the proxy forwards as a `Call` and routes the `ReplyEvent` back
    /// to the original sender.
    #[test]
    fn forward_round_trips_reply_back_to_sender() {
        let (registry, mailer) = fresh_substrate();
        let recorded: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            // TraceObserver produces the `Settled` mail RpcServer's
            // settlement subscription waits on; without it the `Call`
            // never closes with a `ReplyEnd`.
            .with_actor::<TraceObserverCapability>(())
            .with_actor::<TestEchoActor>(())
            .with_actor::<ProxyReplySink>(Arc::clone(&recorded))
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: substrate_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let port = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;

        // Spawn the proxy, dialing this chassis's own RPC server over
        // loopback. A successful `finish()` means `init` connected +
        // handshook.
        chassis
            .spawn_actor::<EngineProxy>(
                Subname::Named("e1"),
                EngineProxyConfig {
                    engine_id: EngineId(Uuid::from_u128(1)),
                    rpc_addr: format!("127.0.0.1:{port}"),
                },
            )
            .finish()
            .expect("proxy spawns + connects");

        let proxy_mailbox = chassis
            .resolve_actor::<EngineProxy>("e1")
            .expect("proxy resolves Live");
        let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Actor>::NAMESPACE);
        let sink_mailbox = mailbox_id_from_name(<ProxyReplySink as Actor>::NAMESPACE);

        // Forge a `ForwardEnvelope` at the proxy, reply-to the sink.
        // `mailer.push` directly (rather than through an actor send) so
        // the test controls the `ReplyTo` the proxy parks.
        let fwd = aether_kinds::ForwardEnvelope {
            mailbox: echo_mailbox,
            kind: <TestEchoRequest as Kind>::ID,
            payload: postcard::to_allocvec(&TestEchoRequest { value: 42 }).unwrap(),
        };
        mailer.push(
            Mail::new(
                proxy_mailbox,
                <aether_kinds::ForwardEnvelope as Kind>::ID,
                postcard::to_allocvec(&fwd).unwrap(),
                1,
            )
            .with_reply_to(ReplyTo::with_correlation(
                ReplyTarget::Component(sink_mailbox),
                777,
            )),
        );

        // Poll for the sink to record the echoed value. The round trip
        // is proxy → server (TCP) → echo → server → proxy (TCP) → sink,
        // all across dispatcher threads — give it a generous deadline.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = *recorded.lock().unwrap() {
                assert_eq!(value, 42, "echoed value routed back through the proxy");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "reply did not route back through the proxy within 5s",
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Spawning a proxy at an address with no RPC server fails at
    /// `init` (the dial errors), surfacing as a spawn `finish()` error
    /// rather than a live-but-dead proxy.
    #[test]
    fn proxy_spawn_fails_when_substrate_unreachable() {
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        // Bind then drop a listener to get a definitely-closed port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let result = chassis
            .spawn_actor::<EngineProxy>(
                Subname::Named("dead"),
                EngineProxyConfig {
                    engine_id: EngineId(Uuid::from_u128(2)),
                    rpc_addr: format!("127.0.0.1:{port}"),
                },
            )
            .finish();
        assert!(
            result.is_err(),
            "spawning a proxy at a closed port should fail at init",
        );
    }
}
