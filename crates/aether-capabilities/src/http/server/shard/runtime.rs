//! The `aether.http.server.shard` runtime half (ADR-0122 identity/runtime
//! split, ADR-0135). The state type and the whole per-connection machine —
//! reader/writer sidecars, parse/render, streaming, websocket — live in the
//! server's shared `runtime` module; this file carries only the shard's
//! `#[runtime] impl`: boot from the supervisor-built [`HttpShardSeed`], the
//! sidecar drain, the stream/websocket/credit handlers, settlement, and the
//! reply-interception fallback.

// `#[handler]` methods take their decoded payload by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the decoded bytes so
// callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// The shard shares the server runtime's one import surface (ADR-0122):
// state + event + helper types come through this glob rather than a
// bespoke list.
#[allow(clippy::wildcard_imports)]
use crate::http::server::runtime::*;

use crate::http::kinds::HttpInboundReady;
use aether_actor::runtime;

use super::HttpDispatchShard;

#[runtime]
impl NativeActor for HttpDispatchShard {
    /// The runtime state this shard boots into (ADR-0135): the connection
    /// table, the in-flight correlation table, and the stream/websocket
    /// tables for this shard's slice of the connections.
    type State = HttpShardState;

    type Config = HttpShardSeed;

    const NAMESPACE: &'static str = "aether.http.server.shard";

    fn init(mut seed: HttpShardSeed, ctx: &mut NativeInitCtx<'_>) -> Result<HttpShardState, BootError> {
        let inbound_rx = seed.inbound_rx.take().expect("HttpShardSeed::inbound_rx consumed exactly once");
        Ok(HttpShardState {
            handler_mailbox: seed.handler_mailbox,
            routes: seed.routes,
            live_connections: seed.live_connections,
            max_request_bytes: seed.max_request_bytes,
            max_header_bytes: seed.max_header_bytes,
            request_timeout: seed.request_timeout,
            keep_alive_timeout: seed.keep_alive_timeout,
            self_mailbox: ctx.self_id(),
            mailer: ctx.mailer(),
            inbound_rx,
            inbound_tx: seed.inbound_tx,
            wake_dirty: seed.wake_dirty,
            connections: HashMap::new(),
            next_conn_id: 0,
            in_flight: HashMap::new(),
            response_stream_window: seed.response_stream_window,
            streams: HashMap::new(),
            request_stream_window: seed.request_stream_window,
            request_streams: HashMap::new(),
            next_stream_id: seed.next_stream_id,
            ws_idle_timeout: seed.ws_idle_timeout,
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // Stop every per-connection reader. Shutting the socket down
        // wakes the blocked `read()`; the reader sees the flag and exits.
        //noinspection DuplicatedCode -- HTTP and RPC own distinct connection state and shutdown semantics.
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
    }

    /// Sidecar wake. Drain every pending inbound event — connection
    /// assignments from the supervisor, plus everything this shard's own
    /// reader / writer sidecars post.
    ///
    /// # Agent
    /// Internal wake mail — not part of the cap's external surface. The
    /// supervisor and this shard's reader sidecars fire this; the handler
    /// drains the mpsc and acts per item.
    #[handler::single]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: HttpInboundReady) {
        WakeSink::arm_for_drain(&state.wake_dirty);
        while let Ok(event) = state.inbound_rx.try_recv() {
            match event {
                InboundEvent::PeerAccepted { stream, peer } => {
                    state.spawn_reader_for_peer(stream, peer);
                }
                InboundEvent::RequestHeadParsed { conn_id, head, handler } => {
                    state.open_requested_stream(ctx, conn_id, head, handler);
                }
                InboundEvent::RequestParsed { conn_id, payload, handler, kind, method, keep_alive, ws_key } => {
                    state.dispatch_prepared(ctx, conn_id, &payload, handler, kind, method, keep_alive, ws_key);
                }
                InboundEvent::RequestBodyChunk { conn_id, body } => {
                    state.forward_request_chunk(ctx, conn_id, body);
                }
                InboundEvent::RequestBodyEnd { conn_id } => {
                    state.end_request_stream(ctx, conn_id);
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
                InboundEvent::WebSocketMessage { conn_id, binary, data } => {
                    state.dispatch_ws_message(ctx, conn_id, binary, data);
                }
                InboundEvent::WebSocketPing { conn_id, payload } => {
                    state.send_ws_pong(conn_id, &payload);
                }
                InboundEvent::WebSocketClose { conn_id, code, reason } => {
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
    #[handler::single]
    fn on_response_chunk(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, chunk: HttpResponseChunk) {
        state.push_chunk(chunk.stream_id, chunk.body);
    }

    /// The handler's stream terminator (ADR-0128). Matched by the payload's
    /// `stream_id` like [`Self::on_response_chunk`].
    ///
    /// # Agent
    /// Not user-callable — a streaming handler sends this once after its
    /// final [`HttpResponseChunk`] to close the stream.
    #[handler::single]
    fn on_response_stream_end(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, end: HttpResponseStreamEnd) {
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
    ///
    /// [`HttpStreamCredit`]: crate::http::kinds::HttpStreamCredit
    #[handler::single]
    fn on_request_credit(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, credit: HttpRequestCredit) {
        state.replenish_reader_credit(credit.stream_id, credit.credit);
    }

    /// Settlement notice. The root corresponds to a dispatched request
    /// we subscribed to; if it settled with no [`HttpServerResponse`]
    /// written, answer `502` (ADR-0108 §5) and clear the entry.
    ///
    /// # Agent
    /// Internal — fires from the settlement registry, not external mail.
    #[handler::single]
    fn on_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Settled) {
        let correlation = mail.root.correlation_id;
        let Some(pending) = state.in_flight.remove(&correlation) else {
            // Already answered (the reply landed first) or never ours.
            return;
        };
        state.respond_and_finish(pending.conn_id, render_status_response(502, "no response from handler"), false);
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
    #[handler::single]
    fn on_websocket_message(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, msg: WebSocketMessage) {
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
    #[handler::single]
    fn on_websocket_close(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, close: WebSocketClose) {
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

    /// Reply interception. Any mail addressed at this shard that isn't one
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
                state.respond_and_finish(
                    pending.conn_id,
                    render_status_response(502, "malformed websocket accept"),
                    false,
                );
            }
            return;
        }
        if env.kind == <HttpResponseStreamOpen as Kind>::ID {
            if let Some(open) = HttpResponseStreamOpen::decode_from_bytes(env.payload.bytes()) {
                state.open_stream(ctx, correlation, pending.conn_id, &open);
            } else {
                state.in_flight.remove(&correlation);
                state.respond_and_finish(pending.conn_id, render_status_response(502, "malformed stream open"), false);
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
            let bytes = render_handler_response(&response, is_head, pending.keep_alive);
            state.in_flight.remove(&correlation);
            // The parked reader writes the bytes (ADR-0135 §3): on
            // keep-alive it loops into the next request; otherwise it
            // exits into the normal ReaderClosed teardown (HTTP/1.0, or
            // `Connection: close`).
            state.respond_and_finish(pending.conn_id, bytes, pending.keep_alive);
        } else {
            // A cap-level error ends the connection (canned responses stay
            // `Connection: close`), which keeps the keep-alive path scoped to
            // the normal success round-trip.
            state.in_flight.remove(&correlation);
            state.respond_and_finish(pending.conn_id, render_status_response(502, "malformed handler response"), false);
        }
    }
}
