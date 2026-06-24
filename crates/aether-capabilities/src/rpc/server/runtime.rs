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

// Sibling / cap-level types named by the state, the helpers, and the
// top-level `#[actor] impl`, reached through the parent module. `super::`
// works because `runtime` is a descendant of `server` (the parent's
// private `use` aliases + the `pub(super)` connection items are visible to
// it). `RpcServerConfig` is named only in the parent's `init` and resolves
// there through the file-root re-export, so it is not pulled here.
use super::connection::{ConnId, ConnState, InboundEvent, run_reader_loop};
use super::{PeerKind, RpcInboundReady, Settled};

// Re-export every substrate / std / cross-crate type the top-level
// `#[actor] impl` body in `mod.rs` names; it reaches them through the
// single `use runtime::*` glob. Types named only by the inherent helper
// methods below ride the same wall (used locally here).
pub use crate::engine::EngineServer;
pub use crate::engine::kinds::{CallSettled, RouteEnvelope};
pub use crate::rpc::{
    Hello, HelloAck, MailEnvelope, MailboxAddress, RpcError, WIRE_VERSION, WireFrame,
};
pub use aether_actor::Addressable;
pub use aether_codec::frame::{FrameError, write_frame};
pub use aether_data::{Kind, KindId, MailId, MailboxId, mailbox_id_from_name};
pub use aether_substrate::Mail;
pub use aether_substrate::actor::native::envelope::Envelope;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::SourceAddr;
pub use aether_substrate::mail::mailer::Mailer;
pub use std::collections::HashMap;
pub use std::io;
pub use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::sync::mpsc;
pub use std::thread::{self, JoinHandle};
pub use std::time::Duration;

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
    pub(super) wire_cid: u64,
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
    pub(super) fn spawn_reader_for_peer(
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
    pub(super) fn dispatch_frame(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        frame: WireFrame,
    ) {
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

    pub(super) fn handle_hello(&mut self, conn_id: ConnId, hello: Hello) {
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

    pub(super) fn handle_call(
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

    pub(super) fn close_connection(&mut self, conn_id: ConnId, reason: &str) {
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

    pub(super) fn write_frame_to(&mut self, conn_id: ConnId, frame: &WireFrame) {
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
