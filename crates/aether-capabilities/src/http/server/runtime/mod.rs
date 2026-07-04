//! The `aether.http.server` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `HttpServerCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` in the parent reaches
//! the state, ctx, and helper types through the single `use runtime::*` glob.
//!
//! Holds the state-bearing `HttpServerCapabilityState` (the 11 fields: the
//! listener port, the accept thread, the per-connection table, the internal
//! mpsc, and the in-flight correlation table), its helper-method impl, the
//! reader/accept sidecar free functions, and the parse/render support types.
//! The reader and accept threads capture only `Arc` / channel / id clones
//! built from locals in `init` (parent) and `spawn_reader_for_peer` (here) —
//! never the cap struct — so the field-home move into this state does not
//! change what the threads capture.

// `#[handler]` methods take their decoded payload by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the decoded bytes so
// callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Parent-level items this module names. `HttpServerConfig` is named by
// `init`'s signature, `HttpServerCapability` is the impl's `Self` type, and
// `HttpServerHandle` is the boot artifact `init` publishes.
use super::{HttpInboundReady, HttpServerCapability, HttpServerConfig, HttpServerHandle, Settled};
use aether_actor::runtime;

pub use std::collections::HashMap;
pub use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::sync::mpsc;
pub use std::thread;
pub use std::time::Duration;

pub use aether_data::{Kind, KindId, MailboxId};
pub use aether_substrate::actor::native::envelope::Envelope;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::mailer::Mailer;
pub use aether_substrate::mail::registry::{MailboxEntry, Registry};

// The parent `#[actor] impl` writes the `502` reply path, so it names
// `HttpServerResponse`; the rest of the kind vocabulary is used only here.
pub use crate::http::kinds::HttpServerResponse;
use crate::http::kinds::{
    HttpHeader, HttpMethod, HttpRequestChunk, HttpRequestCredit, HttpRequestStreamEnd,
    HttpRequestStreamOpen, HttpResponseChunk, HttpResponseStreamEnd, HttpResponseStreamOpen,
    HttpServerRequest, HttpStreamCredit, RegisterRoute, RegisterRouteResult, RegisterRouteSelf,
    UnregisterRoute, UnregisterRouteSelf, UnregisterRoutesAll, WebSocketAccept, WebSocketClose,
    WebSocketMessage,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use aether_substrate::Mail;
use std::io::{self, Read, Write};
use std::mem;
use std::str::from_utf8;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

mod reader;
mod render;
mod routing;
mod state;
mod streaming;
mod types;
mod websocket;

pub use reader::*;
pub use render::*;
pub use routing::*;
pub use state::*;
pub use types::*;
pub use websocket::*;

#[cfg(test)]
mod unit_tests;

#[runtime]
impl NativeActor for HttpServerCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// listener port, the accept thread, the connection table, and the
    /// in-flight correlation table.
    type State = HttpServerCapabilityState;

    type Config = HttpServerConfig;

    const NAMESPACE: &'static str = "aether.http.server";

    fn init(
        config: HttpServerConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<HttpServerCapabilityState, BootError> {
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
        let mailer: Arc<Mailer> = ctx.mailer();
        let self_id = ctx.self_id();
        let wake_kind = KindId(<HttpInboundReady as Kind>::ID.0);

        let accept_sink = WakeSink {
            inbound_tx: inbound_tx.clone(),
            mailer: Arc::clone(&mailer),
            self_id,
            wake_kind,
        };

        // Transport thread below the mail layer — it accepts sockets
        // that carry inbound mail in; no inbound chain to inherit, no
        // settlement umbrella.
        #[allow(clippy::disallowed_methods)]
        let accept_thread = thread::Builder::new()
            .name(format!("aether-http-accept-{port}"))
            .spawn(move || {
                while !accept_shutdown_for_thread.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            if accept_shutdown_for_thread.load(Ordering::Acquire) {
                                drop(stream);
                                break;
                            }
                            if !accept_sink.post(InboundEvent::PeerAccepted { stream, peer }) {
                                break;
                            }
                        }
                        Err(_) => {
                            if accept_shutdown_for_thread.load(Ordering::Acquire) {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        tracing::info!(
            target: "aether_substrate::http_server",
            addr = %config.bind_addr,
            port,
            handler = %config.handler_mailbox,
            "http server bound",
        );

        ctx.publish_handle(HttpServerHandle { local_port: port });

        Ok(HttpServerCapabilityState {
            handler_mailbox: config.handler_mailbox,
            routes: Vec::new(),
            max_request_bytes: config.max_request_bytes,
            max_header_bytes: config.max_header_bytes,
            max_connections: config.max_connections,
            request_timeout: Duration::from_millis(config.request_timeout_millis),
            keep_alive_timeout: Duration::from_millis(config.keep_alive_timeout_millis),
            self_mailbox: self_id,
            mailer,
            listener_port: port,
            accept_shutdown,
            accept_thread: Some(accept_thread),
            inbound_rx,
            inbound_tx,
            connections: HashMap::new(),
            next_conn_id: 0,
            in_flight: HashMap::new(),
            response_stream_window: config.response_stream_window,
            streams: HashMap::new(),
            request_stream_window: config.request_stream_window,
            request_streams: HashMap::new(),
            next_stream_id: 0,
            ws_idle_timeout: Duration::from_millis(config.websocket_idle_timeout_millis),
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // Stop the accept thread; self-connect to unblock its
        // blocking `accept()`.
        state.accept_shutdown.store(true, Ordering::Release);
        if let Ok(addr) = format!("127.0.0.1:{}", state.listener_port).parse::<SocketAddr>() {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
        }
        if let Some(thread) = state.accept_thread.take() {
            let _ = thread.join();
        }
        // Stop every per-connection reader. Shutting the socket down
        // wakes the blocked `read()`; the reader sees the flag and exits.
        for conn in state.connections.values_mut() {
            conn.shutdown.store(true, Ordering::Release);
            let _ = conn.write_half.shutdown(Shutdown::Both);
            if let Some(thread) = conn.reader_thread.take() {
                let _ = thread.join();
            }
        }
        // Stop every response-stream writer (ADR-0128). The socket shutdown
        // above unblocks a write-blocked writer; dropping the sender unblocks
        // a recv-blocked one, so drop it before joining to keep `unwire`
        // prompt (never waiting out the idle-write deadline).
        for (_, mut stream) in state.streams.drain() {
            let writer = stream.writer_thread.take();
            drop(stream);
            if let Some(thread) = writer {
                let _ = thread.join();
            }
        }
        tracing::info!(
            target: "aether_substrate::http_server",
            port = state.listener_port,
            "http server closed",
        );
    }

    /// Sidecar wake. Drain every pending inbound event.
    ///
    /// # Agent
    /// Internal wake mail — not part of the cap's external surface. The
    /// accept / reader sidecars fire this; the handler drains the mpsc
    /// and acts per item.
    #[handler]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: HttpInboundReady) {
        while let Ok(event) = state.inbound_rx.try_recv() {
            match event {
                InboundEvent::PeerAccepted { stream, peer } => {
                    state.spawn_reader_for_peer(stream, peer);
                }
                InboundEvent::RequestHeadParsed { conn_id, head } => {
                    state.decide_request_head(ctx, conn_id, head);
                }
                InboundEvent::RequestParsed { conn_id, request } => {
                    state.dispatch_request(ctx, conn_id, request);
                }
                InboundEvent::RequestBodyChunk { conn_id, body } => {
                    state.forward_request_chunk(ctx, conn_id, body);
                }
                InboundEvent::RequestBodyEnd { conn_id } => {
                    state.end_request_stream(ctx, conn_id);
                }
                InboundEvent::RequestRejected {
                    conn_id,
                    status,
                    message,
                } => {
                    state.write_status_response(conn_id, status, message);
                    state.close_connection(conn_id, "request rejected");
                }
                InboundEvent::ReaderClosed { conn_id, reason } => {
                    state.close_connection(conn_id, &reason);
                }
                InboundEvent::RequestTimedOut { conn_id } => {
                    // A streaming connection's writer thread owns the
                    // idle-write deadline (ADR-0128 §4), so the reader's
                    // response-deadline timeout must not tear an active
                    // stream down — a stream making progress isn't stalled.
                    let streaming = state.streams.values().any(|s| s.conn_id == conn_id);
                    if !streaming {
                        if state.in_flight.values().any(|p| p.conn_id == conn_id) {
                            state.write_status_response(conn_id, 504, "gateway timeout");
                        }
                        state.close_connection(conn_id, "request timeout");
                    }
                }
                InboundEvent::StreamSlotFreed { stream_id } => {
                    state.replenish_credit(ctx, stream_id);
                }
                InboundEvent::StreamFinished { stream_id } => {
                    state.finish_stream(stream_id);
                }
                InboundEvent::WebSocketMessage {
                    conn_id,
                    binary,
                    data,
                } => {
                    state.dispatch_ws_message(ctx, conn_id, binary, data);
                }
                InboundEvent::WebSocketPing { conn_id, payload } => {
                    state.send_ws_pong(conn_id, &payload);
                }
                InboundEvent::WebSocketClose {
                    conn_id,
                    code,
                    reason,
                } => {
                    // Report the close to the handler on its own root, echo the
                    // close frame on the writer (its final write finishes the
                    // stream, tearing the connection down, ADR-0129 §5).
                    state.report_ws_close(ctx, conn_id, code, &reason);
                    state.send_ws_close(conn_id, code, &reason);
                }
            }
        }
    }

    /// One streamed response body chunk from the handler (ADR-0128).
    /// Matched by the payload's `stream_id` against the `streams` table —
    /// not by envelope correlation, since a handler's `ctx.send` mints a
    /// fresh correlation the request's in-flight key would not match.
    ///
    /// # Agent
    /// Not user-callable — a streaming handler sends this after replying
    /// [`HttpResponseStreamOpen`], paced by the cap's
    /// [`HttpStreamCredit`](crate::http::kinds::HttpStreamCredit) grants.
    #[handler]
    fn on_response_chunk(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        chunk: HttpResponseChunk,
    ) {
        state.push_chunk(chunk.stream_id, chunk.body);
    }

    /// The handler's stream terminator (ADR-0128). Matched by the payload's
    /// `stream_id` like [`Self::on_response_chunk`].
    ///
    /// # Agent
    /// Not user-callable — a streaming handler sends this once after its
    /// final [`HttpResponseChunk`] to close the stream.
    #[handler]
    fn on_response_stream_end(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        end: HttpResponseStreamEnd,
    ) {
        state.end_stream(end.stream_id);
    }

    /// A streaming handler's inbound-body credit grant (ADR-0128), the inverse
    /// of the cap → handler [`HttpStreamCredit`]. Matched by the payload's
    /// `stream_id` against the `request_streams` table — a typed handler, not
    /// the reply-interception fallback, so a credit grant is never mistaken for
    /// the handler's final `HttpServerResponse`.
    ///
    /// # Agent
    /// Not user-callable — a streaming handler sends this after receiving
    /// [`HttpRequestStreamOpen`](crate::http::kinds::HttpRequestStreamOpen), as
    /// it drains [`HttpRequestChunk`](crate::http::kinds::HttpRequestChunk)
    /// mails, to let the cap deliver more of the request body.
    #[handler]
    fn on_request_credit(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        credit: HttpRequestCredit,
    ) {
        state.replenish_reader_credit(credit.stream_id, credit.credit);
    }

    /// Claim a route for an explicitly named mailbox (ADR-0130).
    ///
    /// # Agent
    /// `RegisterRoute { prefix, method, kind, mailbox }`. The external
    /// form — an MCP session or test names the handler mailbox
    /// explicitly; it is validated against the registry. An in-process
    /// actor registering itself sends `register_route_self` instead.
    #[handler]
    fn on_register_route(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: RegisterRoute,
    ) -> RegisterRouteResult {
        if let Err(error) = validate_route_mailbox(state.mailer.registry(), payload.mailbox) {
            return RegisterRouteResult::Err { error };
        }
        state.register_route(
            &payload.prefix,
            payload.method,
            payload.kind,
            payload.mailbox,
        )
    }

    /// Claim a route for the *sending* actor (ADR-0130), resolved from
    /// the inbound envelope's host-stamped `Source` — forgery-proof
    /// and gated to in-process actors by construction, mirroring
    /// `aether.input.subscribe_self`.
    ///
    /// # Agent
    /// `RegisterRouteSelf { prefix, method, kind }`, typically sent
    /// from a component's `wire` hook. An external session or remote
    /// engine has no local mailbox and gets an `Err` reply — use
    /// `register_route` with an explicit mailbox instead.
    #[handler]
    fn on_register_route_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: RegisterRouteSelf,
    ) -> RegisterRouteResult {
        match ctx.source_mailbox() {
            Some(mailbox) => {
                state.register_route(&payload.prefix, payload.method, payload.kind, mailbox)
            }
            None => RegisterRouteResult::Err {
                error: "aether.http.server.register_route_self requires a local sender; an \
                        external session or remote engine must use \
                        aether.http.server.register_route with an explicit mailbox"
                    .to_string(),
            },
        }
    }

    /// Release an explicitly named mailbox's route (ADR-0130).
    /// Idempotent.
    ///
    /// # Agent
    /// `UnregisterRoute { prefix, method, mailbox }`.
    #[handler]
    fn on_unregister_route(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: UnregisterRoute,
    ) -> RegisterRouteResult {
        state.unregister_route(&payload.prefix, payload.method, payload.mailbox)
    }

    /// Release the *sending* actor's route (ADR-0130), resolved from
    /// the host-stamped `Source` like `register_route_self`.
    /// Idempotent.
    ///
    /// # Agent
    /// `UnregisterRouteSelf { prefix, method }`.
    #[handler]
    fn on_unregister_route_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: UnregisterRouteSelf,
    ) -> RegisterRouteResult {
        match ctx.source_mailbox() {
            Some(mailbox) => state.unregister_route(&payload.prefix, payload.method, mailbox),
            None => RegisterRouteResult::Err {
                error: "aether.http.server.unregister_route_self requires a local sender; an \
                        external session or remote engine must use \
                        aether.http.server.unregister_route with an explicit mailbox"
                    .to_string(),
            },
        }
    }

    /// Release every route held by a mailbox (ADR-0130). Issued by
    /// `ComponentHostCapability` on `DropComponent`, alongside its
    /// input / lifecycle unsubscribe fan-out, so the route table
    /// doesn't keep dispatching at a dropped trampoline. Idempotent;
    /// fire-and-forget.
    ///
    /// # Agent
    /// `UnregisterRoutesAll { mailbox }`.
    #[handler]
    fn on_unregister_routes_all(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: UnregisterRoutesAll,
    ) {
        state.routes.retain(|r| r.mailbox != payload.mailbox);
    }

    /// Settlement notice. The root corresponds to a dispatched request
    /// we subscribed to; if it settled with no [`HttpServerResponse`]
    /// written, answer `502` (ADR-0108 §5) and clear the entry.
    ///
    /// # Agent
    /// Internal — fires from the settlement registry, not external mail.
    #[handler]
    fn on_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Settled) {
        let correlation = mail.root.correlation_id;
        let Some(pending) = state.in_flight.remove(&correlation) else {
            // Already answered (the reply landed first) or never ours.
            return;
        };
        state.write_status_response(pending.conn_id, 502, "no response from handler");
        state.close_connection(pending.conn_id, "settled without response");
    }

    /// An outbound websocket message from the handler (ADR-0129 §3), routed by
    /// the payload's `stream_id` through the stream table like
    /// [`Self::on_response_chunk`] (ADR-0132) — so the send routes identically
    /// from any causal chain, and a handler can push with no inbound message
    /// in flight. An unknown or torn-down stream drops the message.
    ///
    /// # Agent
    /// Not user-callable — an upgraded connection's handler sends this to
    /// speak to the peer; the cap frames it and drains it under the credit
    /// window.
    #[handler]
    fn on_websocket_message(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        msg: WebSocketMessage,
    ) {
        if let Some(conn_id) = state.streams.get(&msg.stream_id).map(|s| s.conn_id) {
            state.send_ws_message(conn_id, msg.binary, &msg.data);
        } else {
            tracing::debug!(
                target: "aether_substrate::http_server",
                stream = msg.stream_id,
                "outbound websocket message for unknown stream dropped",
            );
        }
    }

    /// A handler-initiated websocket close (ADR-0129 §5), routed by the
    /// payload's `stream_id` like [`Self::on_websocket_message`] (ADR-0132):
    /// the cap writes the close frame on the connection's writer and tears it
    /// down. An unknown or torn-down stream drops the close.
    ///
    /// # Agent
    /// Not user-callable — an upgraded connection's handler sends this to close
    /// the socket.
    #[handler]
    fn on_websocket_close(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        close: WebSocketClose,
    ) {
        if let Some(conn_id) = state.streams.get(&close.stream_id).map(|s| s.conn_id) {
            state.send_ws_close(conn_id, close.code, &close.reason);
        } else {
            tracing::debug!(
                target: "aether_substrate::http_server",
                stream = close.stream_id,
                "outbound websocket close for unknown stream dropped",
            );
        }
    }

    /// Reply interception. Any mail addressed at this cap that isn't one
    /// of the typed wake / settlement kinds is treated as the handler's
    /// reply; if its `correlation_id` matches an in-flight request and
    /// it is an [`HttpServerResponse`], format the HTTP/1.1 response,
    /// write it to the held socket, and close.
    ///
    /// # Agent
    /// Not user-callable — this is the cap's reply-interception path. A
    /// by-value `#[handler]` can't read the inbound `sender.correlation_id`,
    /// so reply correlation goes through this envelope fallback
    /// (ADR-0108 §5). The handler's one-shot reply is either an
    /// [`HttpServerResponse`] (buffered, ADR-0108) or an
    /// [`HttpResponseStreamOpen`] (streamed, ADR-0128); both key on the
    /// request's in-flight correlation id. The mid-stream chunk / end mails
    /// carry a fresh send-correlation and are routed to their own
    /// `#[handler]`s ([`Self::on_response_chunk`] / [`Self::on_response_stream_end`]
    /// / [`Self::on_websocket_message`]), keyed by their explicit `stream_id`
    /// payload — a second correlation regime living beside this one.
    #[fallback]
    fn on_any(state: &mut Self::State, ctx: &mut NativeCtx<'_>, env: &Envelope) {
        let correlation = env.sender.correlation_id;
        let Some(pending) = state.in_flight.get(&correlation).copied() else {
            return;
        };
        // ADR-0129: the handler accepted the upgrade. `WebSocketAccept` is a
        // one-shot reply (correlation-echoed) keyed on the handshake request's
        // in-flight correlation, like `HttpResponseStreamOpen`.
        if env.kind == <WebSocketAccept as Kind>::ID {
            if let Some(accept) = WebSocketAccept::decode_from_bytes(env.payload.bytes()) {
                state.accept_websocket(ctx, correlation, pending.conn_id, pending.handler, &accept);
            } else {
                state.in_flight.remove(&correlation);
                state.write_status_response(pending.conn_id, 502, "malformed websocket accept");
                state.close_connection(pending.conn_id, "malformed websocket accept");
            }
            return;
        }
        if env.kind == <HttpResponseStreamOpen as Kind>::ID {
            if let Some(open) = HttpResponseStreamOpen::decode_from_bytes(env.payload.bytes()) {
                state.open_stream(ctx, correlation, pending.conn_id, &open);
            } else {
                state.in_flight.remove(&correlation);
                state.write_status_response(pending.conn_id, 502, "malformed stream open");
                state.close_connection(pending.conn_id, "malformed stream open");
            }
            return;
        }
        if env.kind != <HttpServerResponse as Kind>::ID {
            // Unexpected kind with a matching correlation — leave the
            // in-flight entry for the settlement / timeout safety net.
            return;
        }
        if let Some(response) = HttpServerResponse::decode_from_bytes(env.payload.bytes()) {
            let is_head = pending.method == HttpMethod::Head;
            state.write_handler_response(pending.conn_id, &response, is_head, pending.keep_alive);
            state.in_flight.remove(&correlation);
            // A successful keep-alive response holds the connection and
            // releases the reader for the next request; otherwise the
            // connection closes (HTTP/1.0, or `Connection: close`).
            if pending.keep_alive {
                state.resume_connection(pending.conn_id);
            } else {
                state.close_connection(pending.conn_id, "response written");
            }
        } else {
            // A cap-level error ends the connection (canned responses stay
            // `Connection: close`), which keeps the keep-alive path scoped to
            // the normal success round-trip.
            state.write_status_response(pending.conn_id, 502, "malformed handler response");
            state.in_flight.remove(&correlation);
            state.close_connection(pending.conn_id, "malformed handler response");
        }
    }
}
