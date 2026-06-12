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

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds need to be importable at file root for the
// `#[bridge]`-emitted `HandlesKind<K>` markers.
use aether_kinds::{RpcInboundReady, trace::Settled};

// Re-export the cap's config + handle struct at file root for chassis
// builders + embedders that read the bound port.
#[cfg(not(target_arch = "wasm32"))]
pub use server_native::{RpcServerConfig, RpcServerHandle};

use aether_rpc::rpc::PeerKind;

#[aether_actor::bridge(singleton)]
mod server_native {
    use super::{PeerKind, RpcInboundReady, Settled};
    use crate::rpc::{
        Hello, HelloAck, MailEnvelope, MailboxAddress, RpcError, WIRE_VERSION, WireFrame,
    };
    use aether_actor::actor;
    use aether_codec::frame::FrameError;
    use aether_codec::frame::{read_frame, write_frame};
    use aether_data::{Kind, KindId, MailId, MailboxId, mailbox_id_from_name};
    use aether_kinds::{CallSettled, RouteEnvelope};
    use aether_substrate::Mail;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::SourceAddr;
    use aether_substrate::mail::mailer::Mailer;
    use std::collections::HashMap;
    use std::io::{self, BufReader};
    use std::net::Shutdown;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
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
        PeerAccepted {
            stream: TcpStream,
            peer: SocketAddr,
        },
        FrameReceived {
            conn_id: ConnId,
            frame: WireFrame,
        },
        ReaderClosed {
            conn_id: ConnId,
            reason: String,
        },
        /// An inbound frame failed to decode but the reader managed to
        /// keep frame-sync (issue 1271). Today this only fires for a
        /// length-prefix that exceeded the framing cap and whose body
        /// was small enough (`size <= 2 * max`) for the reader to drain
        /// without itself becoming an OOM vector. The dispatcher writes
        /// a `ReplyEnd { cid: 0, result: Err(RpcError::FrameTooLarge) }`
        /// and the connection survives. `cid = 0` is the agreed
        /// sentinel for "wire-level error, no in-flight call id to
        /// match against" — the mcp-side router lifts it as an
        /// out-of-band error against the most recently pending call.
        FrameDecodeError {
            conn_id: ConnId,
            error: RpcError,
        },
        /// An inbound length prefix announced a body larger than
        /// `2 * max_frame_size` (issue 1271). Draining it would
        /// itself defeat the OOM guard the cap was written for, so the
        /// dispatcher writes a structured `Bye` and closes the
        /// connection — the OOM safety property holds, the wire error
        /// is named, and the client gets a clean close instead of a
        /// bare reset.
        FrameDecodeAborted {
            conn_id: ConnId,
            error: RpcError,
        },
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

            // Transport thread below the mail layer — it accepts sockets that carry
            // inbound mail in; no inbound chain to inherit, no settlement umbrella.
            #[allow(clippy::disallowed_methods)]
            let thread = thread::Builder::new()
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
                        } else if accept_shutdown_for_thread.load(Ordering::Acquire) {
                            break;
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
            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
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
                let _ = conn.write_half.shutdown(Shutdown::Read);
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
                    InboundEvent::FrameDecodeError { conn_id, error } => {
                        // The reader kept frame-sync (body drained).
                        // Write a structured `ReplyEnd { cid: 0, Err }`
                        // and leave the connection up so further calls
                        // on this socket still work (issue 1271).
                        //
                        // `cid = 0` is the sentinel: the wire couldn't
                        // be decoded far enough to learn the real cid,
                        // so we report against id 0 and the mcp router
                        // surfaces it as a wire-level out-of-band
                        // failure rather than a per-call settled-Err.
                        tracing::warn!(
                            target: "aether_substrate::rpc",
                            conn = conn_id,
                            error = ?error,
                            "rpc inbound frame decode error; keeping connection alive",
                        );
                        self.write_frame_to(
                            conn_id,
                            &WireFrame::ReplyEnd {
                                cid: 0,
                                result: Err(error),
                            },
                        );
                    }
                    InboundEvent::FrameDecodeAborted { conn_id, error } => {
                        // The announced body was big enough to be its
                        // own OOM vector (size > 2 * max). Write a
                        // structured `Bye` so the peer sees a named
                        // close instead of a bare reset, then tear the
                        // connection down (issue 1271).
                        let reason = match &error {
                            RpcError::FrameTooLarge { size, max } => {
                                format!("frame too large: {size} > {max}")
                            }
                            other => format!("frame decode aborted: {other:?}"),
                        };
                        tracing::warn!(
                            target: "aether_substrate::rpc",
                            conn = conn_id,
                            reason = %reason,
                            "rpc inbound frame too large to drain; closing connection",
                        );
                        self.write_frame_to(
                            conn_id,
                            &WireFrame::Bye {
                                reason: reason.clone(),
                            },
                        );
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
                let result = match CallSettled::decode_from_bytes(env.payload.bytes()) {
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
                from: match env.sender.addr {
                    SourceAddr::Component(id) => Some(MailboxAddress::local(id)),
                    _ => None,
                },
                kind: env.kind,
                correlation_id: Some(entry.wire_cid),
                payload: env.payload.bytes().to_vec(),
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

            // Per-connection transport reader below the mail layer — carries inbound
            // mail in; no inbound chain to inherit, no settlement umbrella.
            #[allow(clippy::disallowed_methods)]
            let thread = match thread::Builder::new()
                .name(format!("aether-rpc-reader-{conn_id}"))
                .spawn(move || {
                    run_reader_loop(
                        read_half,
                        conn_id,
                        &shutdown_for_thread,
                        &inbound_tx,
                        &mailer,
                        self_id,
                        wake_kind,
                    );
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
                // Runtime-name routing: forwarding a wire `Call` to the
                // well-known engines cap (`EngineServer::NAMESPACE`); the server
                // holds opaque MailboxId/KindId/bytes, with no compile-time
                // sibling type to resolve through.
                #[allow(clippy::disallowed_methods)]
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
            let _ = conn.write_half.shutdown(Shutdown::Both);
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
                    FrameError::Io(io_err)
                        if matches!(
                            io_err.kind(),
                            io::ErrorKind::BrokenPipe
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::WriteZero
                        ) =>
                    {
                        "peer hung up"
                    }
                    FrameError::Io(_) => "write error",
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

    /// Per-connection reader thread body. Reads frames from
    /// `read_half` and pushes them onto `inbound_tx`; on an oversize
    /// inbound frame (`FrameError::FrameTooLarge`) drains the body if
    /// it's inside the drain ceiling so the connection survives, or
    /// asks the dispatcher to close it with a structured `Bye` if not
    /// (iamacoffeepot/aether#1271). Returns when the connection closes
    /// (peer EOF, read error, shutdown flag, oversize-abort).
    fn run_reader_loop(
        read_half: TcpStream,
        conn_id: ConnId,
        shutdown: &AtomicBool,
        inbound_tx: &mpsc::Sender<InboundEvent>,
        mailer: &Arc<Mailer>,
        self_id: MailboxId,
        wake_kind: KindId,
    ) {
        let mut reader = BufReader::new(read_half);
        loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match read_frame(&mut reader) {
                Ok(frame) => {
                    if inbound_tx
                        .send(InboundEvent::FrameReceived { conn_id, frame })
                        .is_err()
                    {
                        return;
                    }
                    mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
                }
                Err(FrameError::Io(io_err)) if io_err.kind() == io::ErrorKind::UnexpectedEof => {
                    let _ = inbound_tx.send(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: "eof".into(),
                    });
                    mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
                    return;
                }
                Err(FrameError::FrameTooLarge { size, max }) => {
                    let outcome = handle_oversize_frame(
                        &mut reader,
                        conn_id,
                        size,
                        max,
                        inbound_tx,
                        mailer,
                        self_id,
                        wake_kind,
                    );
                    if outcome.is_terminal() {
                        return;
                    }
                }
                Err(e) => {
                    if shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let _ = inbound_tx.send(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: format!("read error: {e}"),
                    });
                    mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
                    return;
                }
            }
        }
    }

    /// Outcome of [`handle_oversize_frame`]. `Continue` means the
    /// reader resumed frame-sync; `Terminal` means the read loop must
    /// exit (drain failed, partial drain, or oversize-abort the
    /// dispatcher is closing the connection for).
    enum OversizeOutcome {
        Continue,
        Terminal,
    }

    impl OversizeOutcome {
        fn is_terminal(&self) -> bool {
            matches!(self, Self::Terminal)
        }
    }

    /// Body half of [`run_reader_loop`]'s `FrameTooLarge` arm: when
    /// the inbound length prefix exceeds the cap, either drain the
    /// body (if `size <= 2 * max`) so the stream re-syncs and the
    /// connection survives, or post a structured-abort event that
    /// asks the dispatcher to close the connection with a `Bye`.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::cast_possible_truncation)]
    fn handle_oversize_frame(
        reader: &mut BufReader<TcpStream>,
        conn_id: ConnId,
        size: usize,
        max: usize,
        inbound_tx: &mpsc::Sender<InboundEvent>,
        mailer: &Arc<Mailer>,
        self_id: MailboxId,
        wake_kind: KindId,
    ) -> OversizeOutcome {
        let drain_ceiling = max.saturating_mul(2);
        if size > drain_ceiling {
            let event = InboundEvent::FrameDecodeAborted {
                conn_id,
                error: RpcError::FrameTooLarge {
                    size: size as u64,
                    max: max as u64,
                },
            };
            if inbound_tx.send(event).is_err() {
                return OversizeOutcome::Terminal;
            }
            mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
            return OversizeOutcome::Terminal;
        }
        // `take(size)` bounds the drain so a racy / lying peer can't
        // push us past the cap-2x ceiling on bytes we'll allocate
        // scratch for.
        let mut drain = io::sink();
        let mut bounded = io::Read::take(reader, size as u64);
        let Ok(drained) = io::copy(&mut bounded, &mut drain) else {
            let _ = inbound_tx.send(InboundEvent::ReaderClosed {
                conn_id,
                reason: format!("frame too large drain failed: {size} > {max}"),
            });
            mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
            return OversizeOutcome::Terminal;
        };
        if (drained as usize) != size {
            // Peer hung up mid-body.
            let _ = inbound_tx.send(InboundEvent::ReaderClosed {
                conn_id,
                reason: format!("frame too large partial drain: {drained}/{size}"),
            });
            mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
            return OversizeOutcome::Terminal;
        }
        let event = InboundEvent::FrameDecodeError {
            conn_id,
            error: RpcError::FrameTooLarge {
                size: size as u64,
                max: max as u64,
            },
        };
        if inbound_tx.send(event).is_err() {
            return OversizeOutcome::Terminal;
        }
        mailer.push(Mail::new(self_id, wake_kind, Vec::new(), 1));
        OversizeOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    // Test harness resolves echo/target actor mailboxes by their NAMESPACE to
    // address Call frames — reference id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::rpc::{Hello, HelloAck, PeerKind, WIRE_VERSION, WireFrame};
    use crate::test_chassis::{TestChassis, fresh_substrate};
    use aether_codec::frame::{read_frame, write_frame};
    use aether_substrate::chassis::builder::Builder;
    use aether_substrate::chassis::builder::PassiveChassis;
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

    /// Boot a chassis hosting only `RpcServerCapability`, connect a
    /// client `TcpStream` to its OS-picked port, and apply
    /// `read_timeout`. Tests that need additional caps (e.g.
    /// `TestEchoActor`, `TraceDispatchCapability`) build their own
    /// chassis and reach for [`connect_to_rpc_server`] for the
    /// connect / timeout half. Returns `(chassis, stream)`; both must
    /// stay alive for the listener to keep accepting.
    fn boot_with_rpc_server_only(timeout: Duration) -> (PassiveChassis<TestChassis>, TcpStream) {
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("rpc server boots");
        let stream = connect_to_rpc_server(&chassis, timeout);
        (chassis, stream)
    }

    /// Boot a chassis with the deferred-echo actor + trace dispatch
    /// behind the RPC server, connect a client, and complete the
    /// handshake. Shared by the deferred-reply settlement tests. Returns
    /// `(chassis, stream)`; both must stay alive for the listener.
    fn boot_with_deferred_echo(timeout: Duration) -> (PassiveChassis<TestChassis>, TcpStream) {
        use crate::rpc::test_echo::DeferredEchoActor;
        use crate::trace::TraceDispatchCapability;

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<DeferredEchoActor>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");
        let mut stream = connect_to_rpc_server(&chassis, timeout);
        complete_handshake(&mut stream);
        (chassis, stream)
    }

    /// Lift the published `RpcServerHandle`'s `local_port`, open a
    /// `TcpStream`, set `read_timeout`. Shared by every test whose
    /// boot path is more elaborate than `boot_with_rpc_server_only`.
    fn connect_to_rpc_server(
        chassis: &PassiveChassis<TestChassis>,
        timeout: Duration,
    ) -> TcpStream {
        let port = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;
        let stream =
            TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to rpc server");
        stream
            .set_read_timeout(Some(timeout))
            .expect("test: set_read_timeout on TcpStream");
        stream
    }

    /// Send a `Hello` carrying the current `WIRE_VERSION` and drain
    /// the resulting `HelloAck` so subsequent test traffic sees a
    /// clean stream. Tests that want to assert specifically against
    /// the handshake reply (handshake_*_roundtrip,
    /// `wire_version_mismatch_*`) write the `Hello` themselves so the
    /// `HelloAck` / `Bye` can be matched on.
    fn complete_handshake(stream: &mut TcpStream) {
        write_frame(
            stream,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION,
                peer: PeerKind::Client {
                    client_name: "test-client".into(),
                    client_version: "0.0.1".into(),
                },
            }),
        )
        .expect("test: write_frame Hello to rpc server");
        let _: WireFrame =
            read_frame(stream).expect("test: read_frame after Hello returns HelloAck");
    }

    /// Boot a `RpcServerCapability` bound to OS-picked port, connect a
    /// real TCP client, exchange `Hello` for `HelloAck`. Sanity-check
    /// the wire's framing + handshake path end-to-end.
    #[test]
    fn handshake_hello_to_hello_ack_roundtrip() {
        // Specifically tests the handshake path end-to-end, so it
        // writes the `Hello` itself rather than using
        // `complete_handshake` (which would discard the `HelloAck`
        // before the asserts can inspect it).
        let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
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
        let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
        complete_handshake(&mut stream);

        write_frame(&mut stream, &WireFrame::Ping(0x00c0_ffee)).expect("write Ping");
        let reply: WireFrame = read_frame(&mut stream).expect("read Pong");
        assert_eq!(reply, WireFrame::Pong(0x00c0_ffee));
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
        use crate::rpc::{MailEnvelope, MailboxAddress};
        use crate::trace::TraceDispatchCapability;
        use aether_actor::Actor;
        use aether_data::{Kind, mailbox_id_from_name};

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            // TraceObserver folds substrate-wide trace events into per-
            // root counters and fires `Settled { root }` mail at the
            // chassis-mailbox once a root drains. Without it,
            // RpcServer's settlement subscription never wakes and
            // the `Call` never produces a `ReplyEnd`.
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<TestEchoActor>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(5));
        complete_handshake(&mut stream);

        // Fire a Call against the echo actor. cid = 0xabc; the cap
        // correlates and ends with ReplyEnd matching the same cid.
        let echo_payload = postcard::to_allocvec(&TestEchoRequest { value: 42 })
            .expect("test setup: TestEchoRequest serializes via postcard");
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
        .expect("test: write_frame Call to rpc server");

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

    /// iamacoffeepot/aether#1321 regression: a `Call` routed through the
    /// RPC server tags its reply `SourceAddr::Component(rpc_server)`, so
    /// a capability that replies via `HubOutbound::send_reply` (which
    /// only routes `Session` / `EngineMailbox`) drops the reply silently —
    /// the same drop #1316/#1319 fixed for the desktop driver. The
    /// `HeadlessWindowCapability` `Err`-replies on `set_window_mode`; with
    /// the bug present this `Call` would yield a bare `ReplyEnd` and zero
    /// `ReplyEvent`s. Routing through the `Mailer` (the complete router)
    /// pushes the reply back locally to the server's `on_any`, so the
    /// `Err` rides home as a `ReplyEvent` before the `ReplyEnd`.
    #[test]
    fn call_headless_window_set_mode_err_reaches_component_reply() {
        use crate::rpc::{MailEnvelope, MailboxAddress};
        use crate::trace::TraceDispatchCapability;
        use crate::window::HeadlessWindowCapability;
        use aether_actor::Actor;
        use aether_data::{Kind, mailbox_id_from_name};
        use aether_kinds::{SetWindowMode, SetWindowModeResult, WindowMode};

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<HeadlessWindowCapability>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(5));
        complete_handshake(&mut stream);

        let payload = postcard::to_allocvec(&SetWindowMode {
            mode: WindowMode::Windowed,
            width: None,
            height: None,
        })
        .expect("test setup: SetWindowMode serializes via postcard");
        let window_mailbox = mailbox_id_from_name(<HeadlessWindowCapability as Actor>::NAMESPACE);
        write_frame(
            &mut stream,
            &WireFrame::Call {
                cid: Some(0xdef),
                envelope: MailEnvelope {
                    to: MailboxAddress::local(window_mailbox),
                    from: None,
                    kind: <SetWindowMode as Kind>::ID,
                    correlation_id: None,
                    payload,
                },
            },
        )
        .expect("test: write_frame Call to rpc server");

        // The `Err` reply must arrive as a ReplyEvent — the drop this
        // test guards against would leave zero events before ReplyEnd.
        let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
        let envelope = match event {
            WireFrame::ReplyEvent { cid, envelope } => {
                assert_eq!(cid, 0xdef);
                envelope
            }
            other => panic!("expected ReplyEvent, got {other:?}"),
        };
        assert_eq!(envelope.kind, <SetWindowModeResult as Kind>::ID);
        let decoded: SetWindowModeResult =
            postcard::from_bytes(&envelope.payload).expect("decode SetWindowModeResult");
        assert!(
            matches!(decoded, SetWindowModeResult::Err { .. }),
            "headless window cap replies Err, got {decoded:?}"
        );

        let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
        match end {
            WireFrame::ReplyEnd { cid, result } => {
                assert_eq!(cid, 0xdef);
                result.expect("ReplyEnd result Ok");
            }
            other => panic!("expected ReplyEnd, got {other:?}"),
        }
    }

    /// iamacoffeepot/aether#1031 end-to-end: a `Call` against an actor
    /// that replies through the ADR-0093 hold-until-resolve dispatch
    /// (spawned worker -> completion wake -> re-reply) must still
    /// produce a `ReplyEvent` followed by a `ReplyEnd`. The settlement
    /// hold keeps the chain open across the spawn, so the RPC server's
    /// settlement subscription wakes only *after* the deferred reply
    /// arrives — not when the handler returns. Pre-fix the chain settled
    /// the instant `on_deferred_echo` returned and the deferred reply
    /// landed in an already-closed call (no `ReplyEvent`, only a bare
    /// `ReplyEnd`, then the late reply dropped).
    #[test]
    fn call_deferred_echo_settles_after_reply() {
        use crate::rpc::test_echo::{DeferredEchoActor, DeferredEchoReply, DeferredEchoRequest};
        use crate::rpc::{MailEnvelope, MailboxAddress};
        use aether_actor::Actor;
        use aether_data::{Kind, mailbox_id_from_name};

        let (_chassis, mut stream) = boot_with_deferred_echo(Duration::from_secs(5));

        let payload = postcard::to_allocvec(&DeferredEchoRequest { value: 99 })
            .expect("test setup: DeferredEchoRequest serializes via postcard");
        let mailbox = mailbox_id_from_name(<DeferredEchoActor as Actor>::NAMESPACE);
        write_frame(
            &mut stream,
            &WireFrame::Call {
                cid: Some(0xdef),
                envelope: MailEnvelope {
                    to: MailboxAddress::local(mailbox),
                    from: None,
                    kind: <DeferredEchoRequest as Kind>::ID,
                    correlation_id: None,
                    payload,
                },
            },
        )
        .expect("test: write_frame Call to rpc server");

        // The deferred reply arrives as a ReplyEvent — proving the chain
        // stayed open long enough for the spawned worker's reply to be
        // intercepted (not dropped into an already-settled call).
        let event: WireFrame = read_frame(&mut stream).expect("read ReplyEvent");
        let envelope = match event {
            WireFrame::ReplyEvent { cid, envelope } => {
                assert_eq!(cid, 0xdef);
                envelope
            }
            other => panic!("expected ReplyEvent for the deferred reply, got {other:?}"),
        };
        assert_eq!(envelope.kind, <DeferredEchoReply as Kind>::ID);
        let decoded: DeferredEchoReply =
            postcard::from_bytes(&envelope.payload).expect("decode deferred reply");
        assert_eq!(decoded.value, 99);

        // ReplyEnd follows — settlement fired after the deferred reply,
        // not when the handler returned.
        let end: WireFrame = read_frame(&mut stream).expect("read ReplyEnd");
        match end {
            WireFrame::ReplyEnd { cid, result } => {
                assert_eq!(cid, 0xdef);
                result.expect("ReplyEnd result Ok");
            }
            other => panic!("expected ReplyEnd, got {other:?}"),
        }
    }

    /// A `Call` carrying a `DispatchTraced` batch with **two**
    /// `DeferredEchoRequest` envelopes — the empirical `send_mail_traced`
    /// failure shape: each child is itself a deferred-reply path
    /// (spawn → loopback → re-reply), routed through the trace cap rather
    /// than directly. Pre-fix the trace cap dispatched each child via
    /// `ctx.send_envelope_traced` which stamps `reply_to` at the
    /// dispatcher's own mailbox (the `push_envelope_buffered` default);
    /// child deferred replies landed at the trace cap, which has no
    /// handler for the reply kind and no `#[fallback]`, so they were
    /// silently dropped. The wire call closed via the (still correct)
    /// settlement signal with `replies: []`. The fix forwards each
    /// child's `reply_to` to the trace cap's own inbound `reply_target`
    /// (typically the RPC server holding the wire `cid`'s in-flight
    /// entry), so child replies — sync or deferred — bubble through to
    /// the wire as `ReplyEvent`s, and settlement still fires only after
    /// each hold-until-resolve dispatch's hold drops.
    ///
    /// Test asserts: TWO `ReplyEvent`s (one `DeferredEchoReply` per
    /// request), then exactly ONE `ReplyEnd`. Order of the two events is
    /// unspecified (the two deferred-echo handlers run in parallel
    /// behind 50ms sleeps); the test pairs by `value`.
    #[test]
    fn dispatch_traced_with_deferred_replies_routes_each_event_then_settles() {
        use crate::rpc::test_echo::{DeferredEchoActor, DeferredEchoReply, DeferredEchoRequest};
        use crate::rpc::{MailEnvelope, MailboxAddress};
        use crate::trace::TraceDispatchCapability;
        use aether_actor::Actor;
        use aether_data::{Kind, mailbox_id_from_name};
        use aether_kinds::MailEnvelope as TracedEnvelope;
        use aether_kinds::trace::DispatchTraced;

        let (_chassis, mut stream) = boot_with_deferred_echo(Duration::from_secs(10));

        // Build a batched DispatchTraced with two DeferredEchoRequest
        // envelopes, addressed at the deferred-echo actor by name (the
        // trace cap resolves names through the registry).
        let batch = DispatchTraced {
            mails: vec![
                TracedEnvelope {
                    recipient_name: <DeferredEchoActor as Actor>::NAMESPACE.into(),
                    kind_name: <DeferredEchoRequest as Kind>::NAME.into(),
                    payload: postcard::to_allocvec(&DeferredEchoRequest { value: 11 })
                        .expect("encode req 11"),
                    count: 1,
                },
                TracedEnvelope {
                    recipient_name: <DeferredEchoActor as Actor>::NAMESPACE.into(),
                    kind_name: <DeferredEchoRequest as Kind>::NAME.into(),
                    payload: postcard::to_allocvec(&DeferredEchoRequest { value: 22 })
                        .expect("encode req 22"),
                    count: 1,
                },
            ],
        };
        let trace_mailbox = mailbox_id_from_name(<TraceDispatchCapability as Actor>::NAMESPACE);
        let payload = postcard::to_allocvec(&batch).expect("encode DispatchTraced");
        write_frame(
            &mut stream,
            &WireFrame::Call {
                cid: Some(0xbeef),
                envelope: MailEnvelope {
                    to: MailboxAddress::local(trace_mailbox),
                    from: None,
                    kind: <DispatchTraced as Kind>::ID,
                    correlation_id: None,
                    payload,
                },
            },
        )
        .expect("test: write_frame Call DispatchTraced to rpc server");

        // The trace cap's synchronous `DispatchTracedAck::Ok` reply
        // arrives as a ReplyEvent. Drain it before scanning for the two
        // deferred replies — its ordering is well-defined (the trace
        // handler replies before the children run), so we can read it
        // first without an unbound search.
        let mut deferred_values: Vec<u64> = Vec::new();
        let mut saw_ack = false;
        // Drain up to 4 ReplyEvent frames (ack + 2 deferred + safety
        // margin) before the ReplyEnd. Each iteration consumes one
        // frame; the ReplyEnd breaks.
        loop {
            let frame: WireFrame = read_frame(&mut stream).expect("read frame");
            match frame {
                WireFrame::ReplyEvent { cid, envelope } => {
                    assert_eq!(cid, 0xbeef);
                    if envelope.kind == <DeferredEchoReply as Kind>::ID {
                        let decoded: DeferredEchoReply =
                            postcard::from_bytes(&envelope.payload).expect("decode deferred reply");
                        deferred_values.push(decoded.value);
                    } else {
                        // Otherwise this is the DispatchTracedAck::Ok
                        // reply; mark it observed but don't assert on
                        // its payload here (the ack carries the root
                        // MailId; the test's load-bearing assertions are
                        // on the deferred-reply payloads).
                        saw_ack = true;
                    }
                }
                WireFrame::ReplyEnd { cid, result } => {
                    assert_eq!(cid, 0xbeef);
                    result.expect("ReplyEnd result Ok");
                    break;
                }
                other => panic!("expected ReplyEvent / ReplyEnd, got {other:?}"),
            }
        }
        assert!(
            saw_ack,
            "expected DispatchTracedAck reply event before ReplyEnd",
        );
        deferred_values.sort_unstable();
        assert_eq!(
            deferred_values,
            vec![11, 22],
            "expected one DeferredEchoReply per request, sorted by value",
        );
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
        use crate::rpc::{MailEnvelope, MailboxAddress};
        use aether_actor::Actor;
        use aether_data::{Kind, mailbox_id_from_name};

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TestEchoActor>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let mut stream = connect_to_rpc_server(&chassis, Duration::from_secs(2));
        complete_handshake(&mut stream);

        // Fire-and-forget Call (cid = None). The echo actor will
        // still reply, but with cid None there's no in-flight entry
        // so the reply has no matching correlation and gets dropped.
        let echo_payload = postcard::to_allocvec(&TestEchoRequest { value: 7 })
            .expect("test setup: TestEchoRequest serializes via postcard");
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
        .expect("test: write_frame fire-and-forget Call to rpc server");

        // Immediately Ping. If the fire-and-forget Call had leaked
        // reply correlation, a ReplyEvent / ReplyEnd would arrive
        // before the Pong. Asserting we see Pong first proves no leak.
        write_frame(&mut stream, &WireFrame::Ping(0x00c0_ffee))
            .expect("test: write_frame Ping to rpc server");
        let reply: WireFrame = read_frame(&mut stream).expect("read Pong");
        assert_eq!(reply, WireFrame::Pong(0x00c0_ffee));
    }

    /// A `Hello` carrying a mismatched `wire_version` triggers a `Bye`
    /// and connection close on the server side.
    #[test]
    fn wire_version_mismatch_kicks_connection() {
        // Sends a deliberately wrong `wire_version` and asserts the
        // server responds with `Bye`, so it can't use
        // `complete_handshake` (which sends the current version).
        let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(2));
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
        .expect("test: write_frame future-version Hello to rpc server");

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

    /// iamacoffeepot/aether#1271: an inbound frame whose announced
    /// length exceeds the framing cap but is within the drain ceiling
    /// (`size <= 2 * max`) is fail-soft. The server drains the body,
    /// writes a `ReplyEnd { cid: 0, Err(RpcError::FrameTooLarge) }`,
    /// and keeps the connection alive — a follow-up `Ping` round-trips
    /// as `Pong`, proving the session survived.
    #[test]
    fn oversize_frame_replies_with_frame_too_large_and_session_survives() {
        use crate::rpc::RpcError;
        use aether_codec::frame::{MAX_FRAME_SIZE, max_frame_size};
        use std::io::Write;

        let (_chassis, mut stream) = boot_with_rpc_server_only(Duration::from_secs(5));
        complete_handshake(&mut stream);

        // Set the read timeout high — the server has to read the full
        // oversize body off the wire before it can write the error
        // reply, so the read for the ReplyEnd is gated on that drain.
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("set_write_timeout");

        // Announce a body just over the cap, then push that many zero
        // bytes. The cap defaults to 64 MiB (MAX_FRAME_SIZE), and the
        // process-wide accessor caches on first read — so the drain
        // ceiling is exactly `2 * max_frame_size()`. Pick the smallest
        // legal oversize: max + 1.
        let max = max_frame_size();
        assert!(max >= MAX_FRAME_SIZE, "cap accessor lifted below default");
        let oversize: usize = max + 1;
        assert!(
            oversize <= max.saturating_mul(2),
            "test size must be inside the drain ceiling",
        );
        #[allow(clippy::cast_possible_truncation)]
        let prefix = (oversize as u32).to_le_bytes();
        stream
            .write_all(&prefix)
            .expect("write oversize length prefix");
        // Write the body in chunks so a 64 MiB+ payload doesn't single-
        // syscall through.
        let chunk = vec![0u8; 1024 * 1024];
        let mut remaining = oversize;
        while remaining > 0 {
            let n = remaining.min(chunk.len());
            stream
                .write_all(&chunk[..n])
                .expect("write oversize body chunk");
            remaining -= n;
        }

        // The server replies with a structured ReplyEnd carrying
        // FrameTooLarge. cid is 0 (the sentinel for "wire-level error,
        // no in-flight cid to bind to").
        let reply: WireFrame = read_frame(&mut stream).expect("read fail-soft ReplyEnd");
        match reply {
            WireFrame::ReplyEnd { cid, result } => {
                assert_eq!(cid, 0, "fail-soft uses cid=0 sentinel");
                match result {
                    Err(RpcError::FrameTooLarge { size, max: cap }) => {
                        assert_eq!(size, oversize as u64);
                        assert_eq!(cap, max as u64);
                    }
                    other => panic!("expected FrameTooLarge, got {other:?}"),
                }
            }
            other => panic!("expected ReplyEnd, got {other:?}"),
        }

        // Ping/Pong round-trips — the session is still alive.
        write_frame(&mut stream, &WireFrame::Ping(0xfeed_face))
            .expect("write Ping after fail-soft");
        let pong: WireFrame = read_frame(&mut stream).expect("read Pong after fail-soft");
        assert_eq!(pong, WireFrame::Pong(0xfeed_face));
    }

    /// Tiny postcard roundtrip for the new `RpcError::FrameTooLarge`
    /// variant — protects the wire shape against accidental rename /
    /// re-ordering.
    #[test]
    fn rpc_error_frame_too_large_postcard_roundtrips() {
        use crate::rpc::RpcError;
        let err = RpcError::FrameTooLarge {
            size: 99_000_000,
            max: 64 * 1024 * 1024,
        };
        let bytes = postcard::to_allocvec(&err).expect("postcard encode");
        let back: RpcError = postcard::from_bytes(&bytes).expect("postcard decode");
        assert_eq!(err, back);
    }

    fn client_peer_kind() -> PeerKind {
        PeerKind::Client {
            client_name: "rpc-client-test".into(),
            client_version: "0.0.1".into(),
        }
    }

    /// Full socket round-trip: boot `RpcServerCapability` + the echo
    /// actor + `TraceDispatchCapability`, connect a real
    /// [`RpcClient`](aether_rpc::rpc::RpcClient), fire a `Call` carrying a
    /// `TestEchoRequest`, and drain the inbound channel — expect
    /// `ReplyEvent { TestEchoReply }` then `ReplyEnd { Ok }`. This is the
    /// only test exercising the actual TCP client↔server path end to end
    /// (the `RpcClient` half moved to `aether-rpc` per ADR-0102; this
    /// integration test stays here, where the server lives).
    #[test]
    fn call_echo_round_trips_over_the_socket() {
        use crate::rpc::test_echo::{TestEchoActor, TestEchoReply, TestEchoRequest};
        use crate::rpc::{MailEnvelope, MailboxAddress, RpcClient};
        use crate::trace::TraceDispatchCapability;
        use aether_actor::Actor;
        use aether_data::{Kind, mailbox_id_from_name};

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            // TraceObserver fires `Settled { root }` once a dispatched
            // chain drains; without it RpcServer's settlement
            // subscription never wakes and no `ReplyEnd` is written.
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<TestEchoActor>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let port = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;

        // No on_frame work needed — `recv_timeout` returning is the
        // observable signal we care about. iamacoffeepot/aether#835:
        // a prior version asserted `frames_seen >= 2` against an
        // AtomicUsize bumped inside the hook, but the hook is a
        // post-enqueue scheduling kick by design — the test thread can
        // wake from `recv_timeout` before the reader thread reaches
        // `on_frame()`, racing the assertion. End-to-end correctness
        // here is the two `recv_timeout` returns below: ReplyEvent then
        // ReplyEnd.
        let mut conn = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {})
            .expect("client connects + handshakes");

        // The handshake handed back the server's identity.
        match &conn.server {
            PeerKind::Substrate { engine_name, .. } => assert_eq!(engine_name, "test"),
            PeerKind::Client { .. } => panic!("expected Substrate peer kind from server"),
        }

        let echo_payload = postcard::to_allocvec(&TestEchoRequest { value: 42 })
            .expect("test setup: TestEchoRequest serializes via postcard");
        let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Actor>::NAMESPACE);
        let cid = conn
            .client
            .call(MailEnvelope {
                to: MailboxAddress::local(echo_mailbox),
                from: None,
                kind: <TestEchoRequest as Kind>::ID,
                correlation_id: None,
                payload: echo_payload,
            })
            .expect("call writes");

        // First frame back: ReplyEvent carrying the echoed reply.
        // recv_timeout so a hung settlement fails the test instead of
        // blocking forever.
        let event = conn
            .inbound
            .recv_timeout(Duration::from_secs(5))
            .expect("ReplyEvent within 5s");
        let envelope = match event {
            WireFrame::ReplyEvent {
                cid: ev_cid,
                envelope,
            } => {
                assert_eq!(ev_cid, cid);
                envelope
            }
            other => panic!("expected ReplyEvent, got {other:?}"),
        };
        assert_eq!(envelope.kind, <TestEchoReply as Kind>::ID);
        let decoded: TestEchoReply = postcard::from_bytes(&envelope.payload).expect("decode reply");
        assert_eq!(decoded.value, 42);

        // Then ReplyEnd closes the call.
        let end = conn
            .inbound
            .recv_timeout(Duration::from_secs(5))
            .expect("ReplyEnd within 5s");
        match end {
            WireFrame::ReplyEnd {
                cid: end_cid,
                result,
            } => {
                assert_eq!(end_cid, cid);
                result.expect("ReplyEnd result Ok");
            }
            other => panic!("expected ReplyEnd, got {other:?}"),
        }
    }

    /// `Ping(nonce)` round-trips as `Pong(nonce)` over the socket via a
    /// real [`RpcClient`](aether_rpc::rpc::RpcClient).
    #[test]
    fn ping_pongs_over_the_socket() {
        use crate::rpc::RpcClient;

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: test_peer_kind(),
            })
            .build_passive()
            .expect("rpc server boots");

        let port = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;

        let mut conn = RpcClient::connect(&format!("127.0.0.1:{port}"), client_peer_kind(), || {})
            .expect("client connects");

        conn.client.ping(0x00c0_ffee).expect("ping writes");
        let pong = conn
            .inbound
            .recv_timeout(Duration::from_secs(2))
            .expect("Pong within 2s");
        assert_eq!(pong, WireFrame::Pong(0x00c0_ffee));
    }
}
