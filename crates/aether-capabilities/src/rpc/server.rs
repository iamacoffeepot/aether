//! `aether.rpc.server` — generic TCP RPC server capability (issue 750).
//!
//! Singleton actor. Binds a `TcpListener` on the configured addr at
//! init, runs a sidecar accept thread that spawns one reader thread
//! per accepted connection. Reader threads frame postcard
//! length-prefix frames via [`aether_codec::frame`] and push them
//! through an internal mpsc; an `RpcInboundReady` wake mail tells the
//! cap's dispatcher to drain.
//!
//! On `Call`, the cap dispatches the wire-borne envelope via
//! `NativeCtx::send_envelope_as_root` (fresh causal chain — the wake
//! mail is causally unrelated to the wire-borne Call) and subscribes
//! to settlement of the resulting root via
//! `SettlementRegistry::subscribe_settlement_mail`. Any reply mail
//! addressed back at this cap with the dispatch's correlation id
//! gets lifted into a `ReplyEvent` and written to the originating
//! connection; the settlement notice closes the call with a
//! `ReplyEnd`.

// Handler-signature kinds need to be importable at file root for the
// `#[bridge]`-emitted `HandlesKind<K>` markers.
use aether_kinds::{RpcInboundReady, trace::Settled};

// Re-export the cap's config + handle struct at file root for chassis
// builders + embedders that read the bound port.
#[cfg(not(target_arch = "wasm32"))]
pub use server_native::{RpcServerConfig, RpcServerHandle};

use super::wire::PeerKind;

#[aether_actor::bridge(singleton)]
mod server_native {
    use super::{PeerKind, RpcInboundReady, Settled};
    use crate::rpc::wire::{
        Hello, HelloAck, MailEnvelope, MailboxAddress, RpcError, WIRE_VERSION, WireFrame,
    };
    use aether_actor::actor;
    use aether_codec::frame::{read_frame, write_frame};
    use aether_data::{Kind, KindId, MailId, MailboxId, mailbox_id_from_name};
    use aether_kinds::{CallSettled, RouteEnvelope};
    use aether_substrate::Mail;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::ReplyTarget;
    use aether_substrate::mail::mailer::Mailer;
    use std::collections::HashMap;
    use std::io::{self, BufReader};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// Per-connection identifier, monotonic within this cap. Distinct
    /// from the OS-level peer addr (one peer may reconnect; ids stay
    /// unique for the cap's lifetime).
    type ConnId = u64;

    /// Init config for [`RpcServerCapability`].
    ///
    /// `bind_addr` is the address to bind on (e.g. `"127.0.0.1:8910"`,
    /// `"0.0.0.0:0"` to let the OS pick). `peer_kind` identifies this
    /// server to connecting peers via the `HelloAck` reply; chassis
    /// builders supply a `PeerKind::Substrate { engine_name, .. }` for
    /// substrate / hub endpoints.
    pub struct RpcServerConfig {
        pub bind_addr: String,
        pub peer_kind: PeerKind,
    }

    /// Exported handle bundle published at boot. Reachable from the
    /// chassis via `PassiveChassis::handle::<RpcServerHandle>()`;
    /// the load-bearing field is `local_port` so embedders (driver
    /// threads, tests) can connect to the OS-picked port when
    /// `bind_addr` requested port 0.
    #[derive(Clone)]
    pub struct RpcServerHandle {
        pub local_port: u16,
    }

    /// Internal event the accept / reader sidecar threads push to the
    /// cap dispatcher via an mpsc. The matching wake-mail kind is
    /// [`RpcInboundReady`] (empty payload) — the dispatcher's
    /// `on_inbound_ready` handler drains the channel and dispatches
    /// per item.
    enum InboundEvent {
        PeerAccepted { stream: TcpStream, peer: SocketAddr },
        FrameReceived { conn_id: ConnId, frame: WireFrame },
        ReaderClosed { conn_id: ConnId, reason: String },
    }

    /// Per-connection state owned by the cap dispatcher. The reader
    /// sidecar holds `shutdown` + a clone of `write_half` for the
    /// reader-side socket (each thread owns one half of the split).
    struct ConnState {
        peer: SocketAddr,
        /// Dispatcher's half — used for inline writes (`HelloAck`,
        /// `ReplyEvent`, `ReplyEnd`, Pong, Bye).
        write_half: TcpStream,
        /// Reader thread's shutdown flag. Cap flips it + shuts down
        /// the read half to wake the blocked `read()`.
        shutdown: Arc<AtomicBool>,
        /// Reader thread handle. Joined in `unwire`.
        reader_thread: Option<JoinHandle<()>>,
        hello_received: bool,
    }

    /// Bookkeeping for one in-flight call (cid passed `Some` on the
    /// wire). Looked up by the dispatch's auto-minted
    /// `correlation_id` (== `MailId.correlation_id` of the dispatched
    /// envelope, which is also the root id since we always dispatch
    /// as chassis-root via `send_envelope_as_root`).
    #[derive(Copy, Clone)]
    struct InFlight {
        conn_id: ConnId,
        wire_cid: u64,
    }

    /// Singleton RPC server cap. Owns one TCP listener + per-
    /// connection state.
    pub struct RpcServerCapability {
        peer_kind: PeerKind,
        self_mailbox: MailboxId,
        /// Cached `Arc<Mailer>` so per-handler ctxs (`NativeCtx`,
        /// which doesn't expose `mailer()`) can fire wake mails into
        /// the cap from internal helpers — and so the `Call`
        /// dispatcher can pass the same Arc into
        /// `subscribe_settlement_mail`. Init grabs it from
        /// `NativeInitCtx::mailer()`; the cap is single-threaded
        /// post-ADR-0038 so direct storage is fine.
        mailer: Arc<Mailer>,
        listener_port: u16,
        accept_shutdown: Arc<AtomicBool>,
        accept_thread: Option<JoinHandle<()>>,
        inbound_rx: mpsc::Receiver<InboundEvent>,
        inbound_tx: mpsc::Sender<InboundEvent>,
        connections: HashMap<ConnId, ConnState>,
        next_conn_id: ConnId,
        /// Internal-correlation → connection / wire-cid. Populated on
        /// `Call { cid: Some(n) }` dispatch; cleared on settlement.
        in_flight: HashMap<u64, InFlight>,
    }

    #[actor]
    impl NativeActor for RpcServerCapability {
        type Config = RpcServerConfig;
        const NAMESPACE: &'static str = "aether.rpc.server";

        fn init(config: RpcServerConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let listener =
                TcpListener::bind(&config.bind_addr).map_err(|e| BootError::Other(Box::new(e)))?;
            let local_addr = listener
                .local_addr()
                .map_err(|e| BootError::Other(Box::new(e)))?;
            let port = local_addr.port();
            listener
                .set_nonblocking(false)
                .map_err(|e| BootError::Other(Box::new(e)))?;

            let accept_shutdown = Arc::new(AtomicBool::new(false));
            let accept_shutdown_for_thread = Arc::clone(&accept_shutdown);

            let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
            let inbound_tx_for_thread = inbound_tx.clone();

            let mailer: Arc<Mailer> = ctx.mailer();
            let self_id = ctx.self_id();
            let wake_kind = KindId(<RpcInboundReady as Kind>::ID.0);

            let thread = std::thread::Builder::new()
                .name(format!("aether-rpc-accept-{port}"))
                .spawn(move || {
                    while !accept_shutdown_for_thread.load(Ordering::Acquire) {
                        if let Ok((stream, peer)) = listener.accept() {
                            if accept_shutdown_for_thread.load(Ordering::Acquire) {
                                drop(stream);
                                break;
                            }
                            if inbound_tx_for_thread
                                .send(InboundEvent::PeerAccepted { stream, peer })
                                .is_err()
                            {
                                break;
                            }
                            mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
                        } else {
                            if accept_shutdown_for_thread.load(Ordering::Acquire) {
                                break;
                            }
                            continue;
                        }
                    }
                })
                .map_err(|e| BootError::Other(Box::new(e)))?;

            tracing::info!(
                target: "aether_substrate::rpc",
                addr = %config.bind_addr,
                port = port,
                "rpc server bound",
            );

            ctx.publish_handle(RpcServerHandle { local_port: port });

            Ok(Self {
                peer_kind: config.peer_kind,
                self_mailbox: self_id,
                mailer: ctx.mailer(),
                listener_port: port,
                accept_shutdown,
                accept_thread: Some(thread),
                inbound_rx,
                inbound_tx,
                connections: HashMap::new(),
                next_conn_id: 0,
                in_flight: HashMap::new(),
            })
        }

        fn unwire(&mut self, _ctx: &mut NativeCtx<'_>) {
            // Stop the accept thread.
            self.accept_shutdown.store(true, Ordering::Release);
            let addr_str = format!("127.0.0.1:{}", self.listener_port);
            if let Ok(addr) = addr_str.parse::<std::net::SocketAddr>() {
                let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
            }
            if let Some(t) = self.accept_thread.take() {
                let _ = t.join();
            }
            // Stop every per-connection reader. Shutting down the read
            // half wakes the blocked `read()`; the reader sees the
            // shutdown flag and exits.
            for conn in self.connections.values_mut() {
                conn.shutdown.store(true, Ordering::Release);
                let _ = conn.write_half.shutdown(std::net::Shutdown::Read);
                if let Some(t) = conn.reader_thread.take() {
                    let _ = t.join();
                }
            }
            tracing::info!(
                target: "aether_substrate::rpc",
                port = self.listener_port,
                "rpc server closed",
            );
        }

        /// Sidecar wake. Drain every pending inbound event.
        ///
        /// # Agent
        /// Internal wake mail — not part of the cap's external surface.
        /// The accept / reader sidecars fire this to wake the
        /// dispatcher; the handler drains the mpsc and dispatches per
        /// item.
        #[handler]
        fn on_inbound_ready(&mut self, ctx: &mut NativeCtx<'_>, _mail: RpcInboundReady) {
            while let Ok(event) = self.inbound_rx.try_recv() {
                match event {
                    InboundEvent::PeerAccepted { stream, peer } => {
                        self.spawn_reader_for_peer(ctx, stream, peer);
                    }
                    InboundEvent::FrameReceived { conn_id, frame } => {
                        self.dispatch_frame(ctx, conn_id, frame);
                    }
                    InboundEvent::ReaderClosed { conn_id, reason } => {
                        self.close_connection(conn_id, &reason);
                    }
                }
            }
        }

        /// Settlement notice from the chassis. The root corresponds
        /// to a `Call` dispatch we subscribed to; close the call by
        /// writing `ReplyEnd { cid, result: Ok(()) }` and dropping
        /// the in-flight entry.
        ///
        /// # Agent
        /// Internal — fires from `SettlementRegistry::fire_settled`,
        /// not from external mail. Subscribers parked in the registry
        /// receive one of these per settled root.
        #[handler]
        fn on_settled(&mut self, _ctx: &mut NativeCtx<'_>, mail: Settled) {
            let correlation = mail.root.correlation_id;
            let Some(entry) = self.in_flight.remove(&correlation) else {
                // No matching in-flight call. Either we never owned
                // this root or the connection already closed and we
                // cleared eagerly. Either way: drop silently.
                return;
            };
            self.write_frame_to(
                entry.conn_id,
                &WireFrame::ReplyEnd {
                    cid: entry.wire_cid,
                    result: Ok(()),
                },
            );
        }

        /// Catch-all. Any mail addressed at this cap that's not one of
        /// the typed wake / settlement kinds is treated as a reply
        /// mail from a downstream actor; if its `correlation_id`
        /// matches an in-flight call, the cap wraps it as a
        /// `ReplyEvent` and writes to the originating connection.
        ///
        /// # Agent
        /// Not user-callable — this is the cap's reply interception
        /// path. The wire is mail-shaped (issue 750 §wire), so any
        /// kind two peers share is reachable; reply correlation goes
        /// through this fallback.
        #[fallback]
        fn on_any(&mut self, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
            let correlation = env.sender.correlation_id;
            let Some(entry) = self.in_flight.get(&correlation).copied() else {
                tracing::debug!(
                    target: "aether_substrate::rpc",
                    kind = %env.kind_name,
                    correlation,
                    "rpc reply with no matching in-flight call; dropping",
                );
                return;
            };

            // A forwarded engine call (issue 763 P5a) closes when its
            // proxy lifts the substrate's terminal `ReplyEnd` into a
            // `CallSettled` — there's no local chain for `on_settled`
            // to catch. Recognize it here, write the wire `ReplyEnd`,
            // and clear the in-flight entry.
            if env.kind == <CallSettled as Kind>::ID {
                let result = match CallSettled::decode_from_bytes(&env.payload) {
                    Some(CallSettled::Ok) => Ok(()),
                    Some(CallSettled::Err { error }) => Err(RpcError::Other { reason: error }),
                    None => Err(RpcError::Other {
                        reason: "malformed CallSettled payload".into(),
                    }),
                };
                self.write_frame_to(
                    entry.conn_id,
                    &WireFrame::ReplyEnd {
                        cid: entry.wire_cid,
                        result,
                    },
                );
                self.in_flight.remove(&correlation);
                return;
            }

            let envelope = MailEnvelope {
                to: MailboxAddress::local(self.self_mailbox),
                from: match env.sender.target {
                    ReplyTarget::Component(id) => Some(MailboxAddress::local(id)),
                    _ => None,
                },
                kind: env.kind,
                correlation_id: Some(entry.wire_cid),
                payload: env.payload.clone(),
            };
            self.write_frame_to(
                entry.conn_id,
                &WireFrame::ReplyEvent {
                    cid: entry.wire_cid,
                    envelope,
                },
            );
        }
    }

    impl RpcServerCapability {
        /// Allocate a fresh `ConnId`, store the connection's write half,
        /// spin a reader thread for the read half.
        fn spawn_reader_for_peer(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            stream: TcpStream,
            peer: SocketAddr,
        ) {
            let conn_id = self.next_conn_id;
            self.next_conn_id += 1;

            let read_half = match stream.try_clone() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::rpc",
                        peer = %peer,
                        error = %e,
                        "rpc conn: try_clone failed; dropping",
                    );
                    return;
                }
            };
            let write_half = stream;
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_for_thread = Arc::clone(&shutdown);

            let mailer: Arc<Mailer> = Arc::clone(&self.mailer);
            let self_id = self.self_mailbox;
            let wake_kind = KindId(<RpcInboundReady as Kind>::ID.0);
            let inbound_tx = self.inbound_tx.clone();

            let thread = match std::thread::Builder::new()
                .name(format!("aether-rpc-reader-{conn_id}"))
                .spawn(move || {
                    let mut reader = BufReader::new(read_half);
                    loop {
                        if shutdown_for_thread.load(Ordering::Acquire) {
                            break;
                        }
                        let frame: WireFrame = match read_frame(&mut reader) {
                            Ok(f) => f,
                            Err(aether_codec::frame::FrameError::Io(io_err))
                                if io_err.kind() == io::ErrorKind::UnexpectedEof =>
                            {
                                let _ = inbound_tx.send(InboundEvent::ReaderClosed {
                                    conn_id,
                                    reason: "eof".into(),
                                });
                                mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
                                break;
                            }
                            Err(e) => {
                                if shutdown_for_thread.load(Ordering::Acquire) {
                                    break;
                                }
                                let _ = inbound_tx.send(InboundEvent::ReaderClosed {
                                    conn_id,
                                    reason: format!("read error: {e}"),
                                });
                                mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
                                break;
                            }
                        };
                        if inbound_tx
                            .send(InboundEvent::FrameReceived { conn_id, frame })
                            .is_err()
                        {
                            break;
                        }
                        mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
                    }
                }) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::rpc",
                        peer = %peer,
                        error = %e,
                        "rpc reader thread spawn failed",
                    );
                    return;
                }
            };

            self.connections.insert(
                conn_id,
                ConnState {
                    peer,
                    write_half,
                    shutdown,
                    reader_thread: Some(thread),
                    hello_received: false,
                },
            );
            tracing::debug!(
                target: "aether_substrate::rpc",
                conn = conn_id,
                peer = %peer,
                "rpc conn accepted",
            );
        }

        /// Dispatch one incoming frame.
        fn dispatch_frame(&mut self, ctx: &mut NativeCtx<'_>, conn_id: ConnId, frame: WireFrame) {
            match frame {
                WireFrame::Hello(hello) => self.handle_hello(conn_id, hello),
                WireFrame::HelloAck(_) => {
                    // Server doesn't expect HelloAck — only clients do.
                    tracing::debug!(
                        target: "aether_substrate::rpc",
                        conn = conn_id,
                        "received HelloAck on server side; ignoring",
                    );
                }
                WireFrame::Call { cid, envelope } => self.handle_call(ctx, conn_id, cid, envelope),
                WireFrame::ReplyEvent { .. } | WireFrame::ReplyEnd { .. } => {
                    // Server doesn't expect reply frames inbound.
                    tracing::debug!(
                        target: "aether_substrate::rpc",
                        conn = conn_id,
                        "received reply frame on server side; ignoring",
                    );
                }
                WireFrame::Ping(token) => {
                    self.write_frame_to(conn_id, &WireFrame::Pong(token));
                }
                WireFrame::Pong(_) => {
                    // Cap doesn't initiate Pings v1; nothing to track.
                }
                WireFrame::Bye { reason } => {
                    self.close_connection(conn_id, &format!("peer bye: {reason}"));
                }
            }
        }

        fn handle_hello(&mut self, conn_id: ConnId, hello: Hello) {
            if hello.wire_version != WIRE_VERSION {
                self.write_frame_to(
                    conn_id,
                    &WireFrame::Bye {
                        reason: format!(
                            "wire_version mismatch: peer={}, server={WIRE_VERSION}",
                            hello.wire_version,
                        ),
                    },
                );
                self.close_connection(conn_id, "wire_version mismatch");
                return;
            }
            if let Some(conn) = self.connections.get_mut(&conn_id) {
                conn.hello_received = true;
            }
            self.write_frame_to(
                conn_id,
                &WireFrame::HelloAck(HelloAck {
                    wire_version: WIRE_VERSION,
                    server: self.peer_kind.clone(),
                }),
            );
        }

        fn handle_call(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            conn_id: ConnId,
            cid: Option<u64>,
            envelope: MailEnvelope,
        ) {
            // Engine-addressed Calls (issue 763 P5a): relay to the
            // engines cap (`aether.engine`), which owns the
            // `EngineId -> proxy` table and re-emits a `ForwardEnvelope`
            // at the right proxy. The substrate's reply streams back
            // here as a normal reply mail (handled by `on_any` as a
            // `ReplyEvent`); its terminal `ReplyEnd` arrives — via the
            // proxy — as a `CallSettled` (also handled by `on_any`).
            //
            // Crucially this path does NOT subscribe to settlement: the
            // local `RouteEnvelope` chain settles almost immediately,
            // long before the remote substrate replies, so settlement
            // would close the wire call prematurely. The terminal close
            // comes from `CallSettled` instead.
            //
            // On a chassis with no engines cap the `RouteEnvelope`
            // warn-drops and the call never closes — only the hub
            // chassis wires `aether.engine`, and only the hub fields
            // engine-addressed Calls.
            if let Some(engine_id) = envelope.to.engine {
                let route = RouteEnvelope {
                    engine_id: engine_id.0.to_string(),
                    mailbox: envelope.to.mailbox,
                    kind: envelope.kind,
                    payload: envelope.payload,
                };
                // `mailbox_id_from_name` of `EngineServer::NAMESPACE`.
                let engine_cap = mailbox_id_from_name("aether.engine");
                let mail_id = ctx.send_envelope_as_root(
                    engine_cap,
                    <RouteEnvelope as Kind>::ID,
                    &route.encode_into_bytes(),
                );
                if let Some(wire_cid) = cid {
                    self.in_flight
                        .insert(mail_id.correlation_id, InFlight { conn_id, wire_cid });
                }
                return;
            }
            // Dispatch the envelope as a fresh chain. The returned
            // MailId is the new chain's root; if cid is Some, subscribe
            // to its settlement to know when to write ReplyEnd.
            let recipient = envelope.to.mailbox;
            let kind = envelope.kind;
            let payload = envelope.payload;
            let mail_id: MailId = ctx.send_envelope_as_root(recipient, kind, &payload);

            let Some(wire_cid) = cid else {
                // Fire-and-forget at the wire layer. No bookkeeping.
                return;
            };

            // Subscribe to settlement of the dispatched chain so we
            // close the call with a ReplyEnd. Requires the chassis
            // settlement registry — fail loud if not wired.
            let Some(reg) = self.mailer.settlement_registry() else {
                self.write_frame_to(
                    conn_id,
                    &WireFrame::ReplyEnd {
                        cid: wire_cid,
                        result: Err(RpcError::Other {
                            reason: "settlement registry unavailable on this chassis".into(),
                        }),
                    },
                );
                return;
            };
            reg.subscribe_settlement_mail(
                mail_id,
                self.self_mailbox,
                <Settled as Kind>::ID,
                Arc::clone(&self.mailer),
            );
            self.in_flight
                .insert(mail_id.correlation_id, InFlight { conn_id, wire_cid });
        }

        fn close_connection(&mut self, conn_id: ConnId, reason: &str) {
            let Some(mut conn) = self.connections.remove(&conn_id) else {
                return;
            };
            conn.shutdown.store(true, Ordering::Release);
            let _ = conn.write_half.shutdown(std::net::Shutdown::Both);
            // Drop reader_thread without joining inline — the
            // dispatcher must not block on the reader. The thread sees
            // the shutdown flag (or its own EOF) and exits; the
            // JoinHandle drop detaches.
            drop(conn.reader_thread.take());
            // Clear in-flight entries pinned to this connection so we
            // don't write ReplyEvents / ReplyEnds to a dead socket.
            self.in_flight.retain(|_, entry| entry.conn_id != conn_id);
            tracing::debug!(
                target: "aether_substrate::rpc",
                conn = conn_id,
                peer = %conn.peer,
                reason,
                "rpc conn closed",
            );
        }

        fn write_frame_to(&mut self, conn_id: ConnId, frame: &WireFrame) {
            let Some(conn) = self.connections.get_mut(&conn_id) else {
                return;
            };
            if let Err(e) = write_frame(&mut conn.write_half, frame) {
                let reason = match &e {
                    aether_codec::frame::FrameError::Io(io_err)
                        if matches!(
                            io_err.kind(),
                            io::ErrorKind::BrokenPipe
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::WriteZero
                        ) =>
                    {
                        "peer hung up"
                    }
                    aether_codec::frame::FrameError::Io(_) => "write error",
                    _ => "frame encode error",
                };
                tracing::debug!(
                    target: "aether_substrate::rpc",
                    conn = conn_id,
                    error = %e,
                    "rpc frame write failed",
                );
                self.close_connection(conn_id, reason);
            }
        }
    }
}

/// Address-resolution helper. The cap's mailbox id, derived from its
/// `NAMESPACE` via the standard name-hash. Convenience for chassis
/// code that wants to address the cap without round-tripping through
/// a runtime lookup.
#[must_use]
pub fn rpc_server_mailbox_id() -> aether_data::MailboxId {
    use aether_actor::Actor;
    aether_data::mailbox_id_from_name(<RpcServerCapability as Actor>::NAMESPACE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::wire::{Hello, HelloAck, PeerKind, WIRE_VERSION, WireFrame};
    use crate::test_chassis::{TestChassis, fresh_substrate};
    use aether_codec::frame::{read_frame, write_frame};
    use aether_substrate::chassis::builder::Builder;
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    fn test_peer_kind() -> PeerKind {
        PeerKind::Substrate {
            engine_name: "test".into(),
            engine_version: "0.1.0".into(),
            kinds: vec![],
        }
    }

    /// Boot a `RpcServerCapability` bound to OS-picked port, connect a
    /// real TCP client, exchange `Hello` for `HelloAck`. Sanity-check
    /// the wire's framing + handshake path end-to-end.
    #[test]
    fn handshake_hello_to_hello_ack_roundtrip() {
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("rpc server boots");

        let handle = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published");
        let port = handle.local_port;
        assert!(port > 0, "OS-picked port should be non-zero");

        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to rpc server");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        write_frame(
            &mut stream,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION,
                peer: PeerKind::Client {
                    client_name: "test-client".into(),
                    client_version: "0.0.1".into(),
                },
            }),
        )
        .expect("write Hello");

        let reply: WireFrame = read_frame(&mut stream).expect("read HelloAck");
        match reply {
            WireFrame::HelloAck(HelloAck {
                wire_version,
                server,
            }) => {
                assert_eq!(wire_version, WIRE_VERSION);
                match server {
                    PeerKind::Substrate { engine_name, .. } => {
                        assert_eq!(engine_name, "test");
                    }
                    PeerKind::Client { .. } => panic!("expected Substrate peer kind"),
                }
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    /// `Ping(token)` round-trips as `Pong(token)`.
    #[test]
    fn ping_pong_roundtrip() {
        let (registry, mailer) = fresh_substrate();
        let _chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("rpc server boots");

        let port = _chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;
        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to rpc server");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        // Send a Hello first so the handshake completes, then drain the
        // HelloAck reply before the Ping/Pong roundtrip.
        write_frame(
            &mut stream,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION,
                peer: PeerKind::Client {
                    client_name: "test-client".into(),
                    client_version: "0.0.1".into(),
                },
            }),
        )
        .unwrap();
        let _: WireFrame = read_frame(&mut stream).unwrap();

        write_frame(&mut stream, &WireFrame::Ping(0xc0ffee)).expect("write Ping");
        let reply: WireFrame = read_frame(&mut stream).expect("read Pong");
        assert_eq!(reply, WireFrame::Pong(0xc0ffee));
    }

    /// End-to-end Call dispatch: connect, handshake, fire a `Call`
    /// addressed at the test echo actor's `TestEchoRequest` kind,
    /// observe a `ReplyEvent { TestEchoReply }` followed by a
    /// `ReplyEnd { Ok(()) }` when the chain settles. Exercises the
    /// full dispatch / settlement / reply-interception path from
    /// phase 2.
    #[test]
    fn call_echo_round_trip_event_then_end() {
        use crate::rpc::test_echo::{TestEchoActor, TestEchoReply, TestEchoRequest};
        use crate::rpc::wire::{MailEnvelope, MailboxAddress};
        use crate::trace::TraceObserverCapability;
        use aether_actor::Actor;
        use aether_data::{Kind, mailbox_id_from_name};

        let (registry, mailer) = fresh_substrate();
        let _chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            // TraceObserver folds substrate-wide trace events into per-
            // root counters and fires `Settled { root }` mail at the
            // chassis-mailbox once a root drains. Without it,
            // RpcServer's settlement subscription never wakes and
            // the `Call` never produces a `ReplyEnd`.
            .with_actor::<TraceObserverCapability>(())
            .with_actor::<TestEchoActor>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let port = _chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;
        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to rpc server");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // Handshake.
        write_frame(
            &mut stream,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION,
                peer: PeerKind::Client {
                    client_name: "test-client".into(),
                    client_version: "0.0.1".into(),
                },
            }),
        )
        .unwrap();
        let _: WireFrame = read_frame(&mut stream).unwrap();

        // Fire a Call against the echo actor. cid = 0xabc; the cap
        // correlates and ends with ReplyEnd matching the same cid.
        let echo_payload = postcard::to_allocvec(&TestEchoRequest { value: 42 }).unwrap();
        let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Actor>::NAMESPACE);
        write_frame(
            &mut stream,
            &WireFrame::Call {
                cid: Some(0xabc),
                envelope: MailEnvelope {
                    to: MailboxAddress::local(echo_mailbox),
                    from: None,
                    kind: <TestEchoRequest as Kind>::ID,
                    correlation_id: None,
                    payload: echo_payload,
                },
            },
        )
        .unwrap();

        // First frame back should be the ReplyEvent carrying the
        // TestEchoReply with the echoed value.
        let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
        let envelope = match event {
            WireFrame::ReplyEvent { cid, envelope } => {
                assert_eq!(cid, 0xabc);
                envelope
            }
            other => panic!("expected ReplyEvent, got {other:?}"),
        };
        assert_eq!(envelope.kind, <TestEchoReply as Kind>::ID);
        let decoded: TestEchoReply = postcard::from_bytes(&envelope.payload).expect("decode reply");
        assert_eq!(decoded.value, 42);

        // Then the ReplyEnd closes the call.
        let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
        match end {
            WireFrame::ReplyEnd { cid, result } => {
                assert_eq!(cid, 0xabc);
                result.expect("ReplyEnd result Ok");
            }
            other => panic!("expected ReplyEnd, got {other:?}"),
        }
    }

    /// Fire-and-forget `Call { cid: None }` skips reply correlation
    /// entirely — no settlement subscription is created, no
    /// `ReplyEnd` is written. Verify by sending a Call with cid None
    /// at the test echo actor (whose reply would otherwise come back
    /// as a `ReplyEvent` if correlation had leaked) and confirming a
    /// subsequent `Ping(token)` round-trips immediately, which proves
    /// no stale `ReplyEvent` / `ReplyEnd` frames are in the way.
    #[test]
    fn call_without_cid_is_fire_and_forget() {
        use crate::rpc::test_echo::{TestEchoActor, TestEchoRequest};
        use crate::rpc::wire::{MailEnvelope, MailboxAddress};
        use aether_actor::Actor;
        use aether_data::{Kind, mailbox_id_from_name};

        let (registry, mailer) = fresh_substrate();
        let _chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TestEchoActor>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let port = _chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;
        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to rpc server");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        // Handshake.
        write_frame(
            &mut stream,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION,
                peer: PeerKind::Client {
                    client_name: "test-client".into(),
                    client_version: "0.0.1".into(),
                },
            }),
        )
        .unwrap();
        let _: WireFrame = read_frame(&mut stream).unwrap();

        // Fire-and-forget Call (cid = None). The echo actor will
        // still reply, but with cid None there's no in-flight entry
        // so the reply has no matching correlation and gets dropped.
        let echo_payload = postcard::to_allocvec(&TestEchoRequest { value: 7 }).unwrap();
        let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Actor>::NAMESPACE);
        write_frame(
            &mut stream,
            &WireFrame::Call {
                cid: None,
                envelope: MailEnvelope {
                    to: MailboxAddress::local(echo_mailbox),
                    from: None,
                    kind: <TestEchoRequest as Kind>::ID,
                    correlation_id: None,
                    payload: echo_payload,
                },
            },
        )
        .unwrap();

        // Immediately Ping. If the fire-and-forget Call had leaked
        // reply correlation, a ReplyEvent / ReplyEnd would arrive
        // before the Pong. Asserting we see Pong first proves no leak.
        write_frame(&mut stream, &WireFrame::Ping(0xc0ffee)).unwrap();
        let reply: WireFrame = read_frame(&mut stream).expect("read Pong");
        assert_eq!(reply, WireFrame::Pong(0xc0ffee));
    }

    /// A `Hello` carrying a mismatched `wire_version` triggers a `Bye`
    /// and connection close on the server side.
    #[test]
    fn wire_version_mismatch_kicks_connection() {
        let (registry, mailer) = fresh_substrate();
        let _chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("rpc server boots");

        let port = _chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;
        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to rpc server");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        write_frame(
            &mut stream,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION + 1,
                peer: PeerKind::Client {
                    client_name: "future-client".into(),
                    client_version: "9.9.9".into(),
                },
            }),
        )
        .unwrap();

        let reply: WireFrame = read_frame(&mut stream).expect("read Bye");
        match reply {
            WireFrame::Bye { reason } => {
                assert!(
                    reason.contains("wire_version"),
                    "Bye reason should mention wire_version: {reason}",
                );
            }
            other => panic!("expected Bye, got {other:?}"),
        }
    }
}
