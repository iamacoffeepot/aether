//! The `aether.rpc.server` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;`
//! declaration in the parent carries the gate), so a transport-only build
//! of the [`RpcServerCapability`](super::RpcServerCapability) identity never
//! names these types nor pulls `aether_substrate`. The substrate-typed
//! imports are gated once by this module rather than line-by-line; the
//! `#[actor] impl` in the parent reaches the state, ctx types, the
//! `RpcServerHandle` boot artifact, and the per-connection helpers through
//! the single `use runtime::*` glob.
//!
//! The accept thread (spawned in `init`) and the per-connection reader
//! threads (spawned in [`RpcServerState::spawn_reader_for_peer`]) capture
//! only cloned channel / `Arc<Mailer>` / `MailboxId` handles built in
//! `init` or cloned out of the state — never the `RpcServerState` value —
//! so the thread spawn / wake-mail / settlement-subscription / shutdown
//! path transfers from the pre-split cap struct unchanged.

// `#[handler]` methods take their decoded payload by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the decoded bytes so
// callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Sibling / cap-level types named by the state, the helpers, and the
// `#[runtime] impl NativeActor` block below, reached through the parent
// module. `super::` works because `runtime` is a descendant of `server` (the
// parent's private `use` aliases + the `pub(super)` connection items are
// visible to it). `RpcServerConfig` is named by `init`'s signature; the cap
// struct `RpcServerCapability` is the impl's `Self` type.
use super::connection::{ConnId, ConnState, InboundEvent, run_reader_loop};
use super::{PeerKind, RpcInboundReady, RpcServerCapability, RpcServerConfig, Settled};
use aether_actor::runtime;

// Re-export every substrate / std / cross-crate type the top-level
// `#[actor] impl` body in `mod.rs` names; it reaches them through the
// single `use runtime::*` glob. Types named only by the inherent helper
// methods below ride the same wall (used locally here).
pub(super) use crate::engine::EngineServer;
pub(super) use crate::engine::kinds::{CallSettled, RouteEnvelope};
pub(super) use crate::rpc::{
    Hello, HelloAck, MailEnvelope, MailboxAddress, RpcError, WIRE_VERSION, WireFrame,
};
pub(super) use aether_actor::Addressable;
pub use aether_codec::frame::{FrameError, write_frame};
pub(super) use aether_data::{Kind, KindId, MailId, MailboxId, mailbox_id_from_name};
pub(super) use aether_substrate::Mail;
pub(super) use aether_substrate::actor::native::envelope::Envelope;
pub(super) use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub(super) use aether_substrate::chassis::error::BootError;
pub(super) use aether_substrate::mail::SourceAddr;
pub(super) use aether_substrate::mail::mailer::Mailer;
pub(super) use std::collections::HashMap;
pub(super) use std::io;
pub(super) use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
pub(super) use std::sync::Arc;
pub(super) use std::sync::atomic::{AtomicBool, Ordering};
pub(super) use std::sync::mpsc;
pub(super) use std::thread::{self, JoinHandle};
pub(super) use std::time::Duration;

/// Exported handle bundle published at boot. Reachable from the
/// chassis via `PassiveChassis::handle::<RpcServerHandle>()`;
/// the load-bearing field is `local_port` so embedders (driver
/// threads, tests) can connect to the OS-picked port when
/// `bind_addr` requested port 0.
#[derive(Clone)]
pub struct RpcServerHandle {
    pub local_port: u16,
}

/// Bookkeeping for one in-flight call (cid passed `Some` on the
/// wire). Looked up by the dispatch's auto-minted
/// `correlation_id` (== `MailId.correlation_id` of the dispatched
/// envelope, which is also the root id since we always dispatch
/// as chassis-root via `send_envelope_as_root`). Fields are
/// `pub(super)` so the parent's `on_settled` / `on_any` handlers can
/// read them after `remove` / `get`.
#[derive(Copy, Clone)]
pub(super) struct InFlight {
    pub(super) conn_id: ConnId,
    wire_cid: u64,
}

/// `aether.rpc.server` runtime state (ADR-0122 split). Owns one TCP
/// listener's bookkeeping plus per-connection state. The dispatcher holds
/// this as the cap's state and routes envelopes through the macro-emitted
/// `Dispatch` impl; the addressing identity is the distinct ZST
/// [`RpcServerCapability`](super::RpcServerCapability). Living in this
/// private module keeps it `pub`-enough to satisfy the `NativeActor::State`
/// interface without exposing it as crate-public API; fields are
/// `pub(super)` so the parent's handlers / `init` / `unwire` reach them.
pub struct RpcServerState {
    pub(super) peer_kind: PeerKind,
    pub(super) self_mailbox: MailboxId,
    /// Cached `Arc<Mailer>` so per-handler ctxs (`NativeCtx`,
    /// which doesn't expose `mailer()`) can fire wake mails into
    /// the cap from internal helpers — and so the `Call`
    /// dispatcher can pass the same Arc into
    /// `subscribe_settlement_mail`. Init grabs it from
    /// `NativeInitCtx::mailer()`; the cap is single-threaded
    /// post-ADR-0038 so direct storage is fine.
    pub(super) mailer: Arc<Mailer>,
    pub(super) listener_port: u16,
    pub(super) accept_shutdown: Arc<AtomicBool>,
    pub(super) accept_thread: Option<JoinHandle<()>>,
    pub(super) inbound_rx: mpsc::Receiver<InboundEvent>,
    pub(super) inbound_tx: mpsc::Sender<InboundEvent>,
    pub(super) connections: HashMap<ConnId, ConnState>,
    pub(super) next_conn_id: ConnId,
    /// Internal-correlation → connection / wire-cid. Populated on
    /// `Call { cid: Some(n) }` dispatch; cleared on settlement.
    pub(super) in_flight: HashMap<u64, InFlight>,
}

impl RpcServerState {
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
            let engine_cap = mailbox_id_from_name(<EngineServer as Addressable>::NAMESPACE);
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

#[runtime]
impl NativeActor for RpcServerCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// state-bearing struct holding the TCP listener bookkeeping +
    /// per-connection state.
    type State = RpcServerState;
    type Config = RpcServerConfig;
    const NAMESPACE: &'static str = "aether.rpc.server";

    fn init(
        config: RpcServerConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<RpcServerState, BootError> {
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
                        mailer.push(Mail::new(
                            self_id,
                            wake_kind,
                            RpcInboundReady::default().encode_into_bytes(),
                            1,
                        ));
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

        Ok(RpcServerState {
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

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // Stop the accept thread.
        state.accept_shutdown.store(true, Ordering::Release);
        let addr_str = format!("127.0.0.1:{}", state.listener_port);
        if let Ok(addr) = addr_str.parse::<SocketAddr>() {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
        }
        if let Some(t) = state.accept_thread.take() {
            let _ = t.join();
        }
        // Stop every per-connection reader. Shutting down the read
        // half wakes the blocked `read()`; the reader sees the
        // shutdown flag and exits.
        for conn in state.connections.values_mut() {
            conn.shutdown.store(true, Ordering::Release);
            let _ = conn.write_half.shutdown(Shutdown::Read);
            if let Some(t) = conn.reader_thread.take() {
                let _ = t.join();
            }
        }
        tracing::info!(
            target: "aether_substrate::rpc",
            port = state.listener_port,
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
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: RpcInboundReady) {
        while let Ok(event) = state.inbound_rx.try_recv() {
            match event {
                InboundEvent::PeerAccepted { stream, peer } => {
                    state.spawn_reader_for_peer(ctx, stream, peer);
                }
                InboundEvent::FrameReceived { conn_id, frame } => {
                    state.dispatch_frame(ctx, conn_id, frame);
                }
                InboundEvent::ReaderClosed { conn_id, reason } => {
                    state.close_connection(conn_id, &reason);
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
                    state.write_frame_to(
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
                    state.write_frame_to(
                        conn_id,
                        &WireFrame::Bye {
                            reason: reason.clone(),
                        },
                    );
                    state.close_connection(conn_id, &reason);
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
    fn on_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Settled) {
        let correlation = mail.root.correlation_id;
        let Some(entry) = state.in_flight.remove(&correlation) else {
            // No matching in-flight call. Either we never owned
            // this root or the connection already closed and we
            // cleared eagerly. Either way: drop silently.
            return;
        };
        state.write_frame_to(
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
    fn on_any(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
        let correlation = env.sender.correlation_id;
        let Some(entry) = state.in_flight.get(&correlation).copied() else {
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
            state.write_frame_to(
                entry.conn_id,
                &WireFrame::ReplyEnd {
                    cid: entry.wire_cid,
                    result,
                },
            );
            state.in_flight.remove(&correlation);
            return;
        }

        let envelope = MailEnvelope {
            to: MailboxAddress::local(state.self_mailbox),
            from: match env.sender.addr {
                SourceAddr::Component(id) => Some(MailboxAddress::local(id)),
                _ => None,
            },
            kind: env.kind,
            correlation_id: Some(entry.wire_cid),
            payload: env.payload.bytes().to_vec(),
        };
        state.write_frame_to(
            entry.conn_id,
            &WireFrame::ReplyEvent {
                cid: entry.wire_cid,
                envelope,
            },
        );
    }
}
