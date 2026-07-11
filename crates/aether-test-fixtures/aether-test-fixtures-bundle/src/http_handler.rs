//! Reference HTTP handler fixture for the `serving-http` e2e test and
//! recipe (issue 1762, ADR-0108). Not a demo, not exemplary — its only
//! job is to prove the `aether.http.server` guest load path end to end:
//! `HttpServerCapability` dispatches an `HttpServerRequest` here; this
//! actor path-matches and replies `HttpServerResponse`; the cap formats
//! the HTTP/1.1 response and writes it to the client socket.
//!
//! Behaviour:
//!
//! - Any request → 200, echoing the request path in the body
//!   (`hello from aether: {path}`)
//!
//! Registered at `aether.component/aether.embedded:test.web` after load.
//! The e2e test configures `HttpServerConfig.handler_mailbox` to that
//! address and then fires real `TcpStream` requests at the bound port.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references. A stateless handler that
// ignores `self` is correct but triggers `unused_self`.
#![allow(clippy::needless_pass_by_value, clippy::unused_self)]

use std::collections::BTreeMap;

use aether_actor::{ActorInitError, Manual, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::ComponentHostCapability;
use aether_capabilities::http;
use aether_capabilities::http::HttpServerCapability;
use aether_capabilities::http::kinds::{
    HttpResponseStreamOpen, HttpServerRequest, HttpServerResponse, HttpStreamCredit, RegisterRouteSelf,
    WebSocketAccept, WebSocketClose, WebSocketMessage,
};
use aether_capabilities::http::{ResponseStream, WebSocketStream};
use aether_data::{Kind, MailboxId};
use aether_kinds::DropComponent;

pub struct HttpHandler;

#[actor]
impl WasmActor for HttpHandler {
    const NAMESPACE: &'static str = "test.web";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HttpHandler)
    }

    /// Route an inbound HTTP request to a status + body and reply the
    /// formatted response. The HTTP server cap writes the reply to the
    /// waiting client socket.
    ///
    /// # Agent
    /// Not sent manually — the `aether.http.server` cap dispatches it
    /// on every inbound request. Configure `HttpServerConfig.handler_mailbox`
    /// to `"aether.component/aether.embedded:test.web"` to route here.
    #[handler::single]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, req: HttpServerRequest) -> HttpServerResponse {
        HttpServerResponse {
            status: 200,
            headers: Vec::new(),
            body: format!("hello from aether: {}", req.path).into_bytes(),
        }
    }
}

/// The number of body chunks [`StreamingHttpHandler`] emits, above a typical
/// credit window so the e2e round trip exercises credit replenishment.
const STREAM_CHUNK_COUNT: u32 = 20;

/// Reference response-streaming handler fixture (ADR-0128) for the
/// `serving-http` streaming e2e test. It replies `HttpResponseStreamOpen`
/// instead of `HttpServerResponse`, emits `STREAM_CHUNK_COUNT` chunks paced
/// against the cap's `HttpStreamCredit` grants, and terminates with
/// `HttpResponseStreamEnd`. Each chunk is `"chunk-{i}\n"`, so the client
/// reassembles a deterministic body.
///
/// Registered at `aether.component/aether.embedded:test.web_stream` after load.
/// Spend one `HttpStreamCredit` grant: arm the stream handle on the first
/// grant (ADR-0133 — the counterparty that dispatched it, so chunks flow
/// back to whoever paced the stream rather than a hard-coded cap
/// singleton), emit up to `credit.credit` more chunks, and terminate once
/// all [`STREAM_CHUNK_COUNT`] have gone out. Shared verbatim by
/// [`StreamingHttpHandler`] and [`RoutedStreamingHttpHandler`] so their
/// e2e comparison isolates the dispatch path, not the behavior.
fn spend_credit(
    stream_slot: &mut Option<ResponseStream>,
    next_index: &mut u32,
    ended: &mut bool,
    ctx: &mut WasmCtx<'_, Manual>,
    credit: &HttpStreamCredit,
) {
    let stream = match *stream_slot {
        Some(stream) => stream,
        None => match ResponseStream::from_credit(ctx, credit) {
            Some(stream) => *stream_slot.insert(stream),
            None => return,
        },
    };
    let mut budget = credit.credit;
    while budget > 0 && *next_index < STREAM_CHUNK_COUNT {
        stream.chunk(ctx, format!("chunk-{}\n", *next_index).into_bytes());
        *next_index += 1;
        budget -= 1;
    }
    if *next_index >= STREAM_CHUNK_COUNT && !*ended {
        stream.end(ctx);
        *ended = true;
    }
}

pub struct StreamingHttpHandler {
    /// The stream this handler is feeding (ADR-0133), captured from the
    /// first credit mail — the counterparty that dispatched it plus the
    /// stream id. `None` until that first grant arms it.
    stream: Option<ResponseStream>,
    /// Index of the next chunk to emit.
    next_index: u32,
    /// Whether the terminator has been sent.
    ended: bool,
}

#[actor]
impl WasmActor for StreamingHttpHandler {
    const NAMESPACE: &'static str = "test.web_stream";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(StreamingHttpHandler { stream: None, next_index: 0, ended: false })
    }

    /// Open a streamed `200` response. The body arrives later, one chunk per
    /// unit of credit the cap grants.
    ///
    /// # Agent
    /// Not sent manually — the `aether.http.server` cap dispatches it on
    /// every inbound request. Route here by pointing
    /// `HttpServerConfig.handler_mailbox` at
    /// `"aether.component/aether.embedded:test.web_stream"`.
    #[handler::single]
    //noinspection DuplicatedCode -- actor macros require one request handler per fixture actor type.
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, _req: HttpServerRequest) -> HttpResponseStreamOpen {
        self.next_index = 0;
        self.ended = false;
        HttpResponseStreamOpen { status: 200, headers: Vec::new() }
    }

    /// Spend the granted credit: emit up to `credit.credit` more chunks, then
    /// terminate once all `STREAM_CHUNK_COUNT` have gone out.
    ///
    /// # Agent
    /// Not sent manually — the cap sends one `HttpStreamCredit` per freed
    /// window slot; the handler emits at most that many `HttpResponseChunk`s
    /// in response.
    #[handler::manual]
    //noinspection DuplicatedCode -- actor macros require one credit handler per fixture actor type.
    fn on_credit(&mut self, ctx: &mut WasmCtx<'_, Manual>, credit: HttpStreamCredit) {
        spend_credit(&mut self.stream, &mut self.next_index, &mut self.ended, ctx, &credit);
    }
}

/// Reference websocket handler fixture (ADR-0129 / ADR-0132) for the
/// `serving-http` websocket e2e test. On an upgrade request it opts in with
/// `WebSocketAccept` (any inbound request is treated as an upgrade — the cap
/// only dispatches here after it has validated the RFC 6455 handshake). On the
/// first `HttpStreamCredit` grant — before any peer traffic — it pushes an
/// unsolicited text greeting to the granted `stream_id`, exercising the
/// ADR-0132 push path from a chain with no inbound websocket message.
/// Thereafter it echoes each inbound `WebSocketMessage` straight back
/// (preserving `stream_id` and the text/binary flag), except the text message
/// `misroute`, which it echoes to a deliberately wrong `stream_id` so the e2e
/// can assert the cap drops an unknown-stream send without tearing the
/// connection down.
///
/// Registered at `aether.component/aether.embedded:test.web_socket` after load.
pub struct WebSocketHandler {
    /// Per-connection upgraded streams (ADR-0133), keyed by `stream_id` and
    /// inserted on that connection's accept-time credit grant. Map
    /// membership is the greeted flag: a `stream_id` present here has
    /// already received its greeting.
    connections: BTreeMap<u64, WebSocketStream>,
}

#[actor]
impl WasmActor for WebSocketHandler {
    const NAMESPACE: &'static str = "test.web_socket";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WebSocketHandler { connections: BTreeMap::new() })
    }

    /// Accept every upgrade the cap routes here (the cap has already validated
    /// the handshake). A real handler would apply auth / path policy first.
    ///
    /// # Agent
    /// Not sent manually — the `aether.http.server` cap dispatches an
    /// `HttpServerRequest` for a websocket upgrade; replying `WebSocketAccept`
    /// completes the handshake, `HttpServerResponse` declines it.
    #[handler::single]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, _req: HttpServerRequest) -> WebSocketAccept {
        WebSocketAccept { subprotocol: None, headers: Vec::new() }
    }

    /// Echo one inbound message back to the peer by its `stream_id`
    /// (ADR-0132). The `misroute` text message instead echoes to a wrong
    /// stream id, which the cap must drop without touching the connection.
    ///
    /// # Agent
    /// Not sent manually — the cap dispatches one per complete inbound
    /// websocket message on the upgraded connection.
    #[handler::single]
    fn on_message(&mut self, ctx: &mut WasmCtx<'_>, msg: WebSocketMessage) {
        // The connection handle was captured on this connection's
        // accept-time credit grant (ADR-0132/ADR-0133), which always
        // precedes peer traffic on that connection.
        let Some(stream) = self.connections.get(&msg.stream_id).copied() else {
            return;
        };
        if !msg.binary && msg.data == b"misroute" {
            // Echo to a deliberately wrong stream id on the same
            // counterparty — the cap must drop an unknown-stream send
            // without tearing the connection down.
            let misrouted =
                WebSocketStream { counterparty: stream.counterparty, stream_id: msg.stream_id.wrapping_add(1000) };
            misrouted.message(ctx, msg.binary, msg.data);
            return;
        }
        stream.message(ctx, msg.binary, msg.data);
    }

    /// The cap's outbound send window (ADR-0128 / ADR-0129 §3). The first
    /// grant is the accept-time window (ADR-0132): it names this connection's
    /// `stream_id`, so push the unsolicited greeting here — no inbound
    /// websocket message exists yet, proving outbound routing holds off the
    /// inbound chain. Beyond that the echo never outruns the window, so no
    /// credit is tracked.
    ///
    /// # Agent
    /// Not sent manually — the cap grants credit as writer slots free.
    #[handler::manual]
    fn on_credit(&mut self, ctx: &mut WasmCtx<'_, Manual>, credit: HttpStreamCredit) {
        // ADR-0133: capture the connection handle from the accept-time
        // credit grant — its counterparty is whoever owns the socket. A
        // repeat grant for a known stream_id is a no-op: this connection
        // has already been greeted.
        if self.connections.contains_key(&credit.stream_id) {
            return;
        }
        let Some(stream) = WebSocketStream::from_credit(ctx, &credit) else {
            return;
        };
        self.connections.insert(credit.stream_id, stream);
        stream.message(ctx, false, b"server greeting".to_vec());
    }

    /// A peer-initiated close (ADR-0129 §5). Evict the connection's state so
    /// a long-lived reference handler does not leak per-connection state as
    /// sockets churn — the cap has already echoed the close frame and is
    /// tearing the connection down.
    ///
    /// # Agent
    /// Not sent manually — the cap reports a peer close here.
    #[handler::single]
    fn on_close(&mut self, _ctx: &mut WasmCtx<'_>, close: WebSocketClose) {
        self.connections.remove(&close.stream_id);
    }
}

/// Routed sibling of [`HttpHandler`] for the ADR-0130 drop-purge e2e
/// test, authored through the typed route surface (`#[http::router]` /
/// `#[http::route]`, ADR-0131): the macro mints the route's kind and
/// injects the `register_route_self` registration, so this fixture is
/// the wasm32 + `Lifecycle<S>` universality proof for the macro layer.
/// It replies a fixed tag for `/routed`. `GET /routed/drop` doubles as
/// the test's mail bridge into the chassis — the request body carries a
/// decimal trampoline mailbox id, and the handler forwards a
/// [`DropComponent`] for it to `aether.component` (detached: the drop
/// teardown is not part of the request's causal chain), so the test can
/// drop this component from outside without a chassis-level mail surface.
pub struct RoutedHttpHandler;

#[http::router]
#[actor]
impl WasmActor for RoutedHttpHandler {
    const NAMESPACE: &'static str = "test.routed_web";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(RoutedHttpHandler)
    }

    /// Reply a fixed tag for anything under `/routed`; on
    /// `/routed/drop` also forward a [`DropComponent`] for the mailbox
    /// id named in the request body. The request arrives via the
    /// identity [`HttpServerRequest`] extractor; `ctx` derefs to
    /// `WasmCtx`, so the detached `DropComponent` send reads exactly as
    /// an ordinary handler's.
    ///
    /// # Agent
    /// Not sent manually — dispatched by `aether.http.server` for the
    /// `/routed` prefix this actor claims (the `#[http::route]` macro
    /// injects the registration into `wire`).
    #[http::route(any, "/routed")]
    fn on_routed(&mut self, ctx: http::Ctx<'_, WasmCtx<'_>>, req: HttpServerRequest) -> HttpServerResponse {
        if req.path == "/routed/drop" {
            let target = String::from_utf8_lossy(&req.body).trim().parse::<u64>();
            let Ok(raw_id) = target else {
                return HttpServerResponse {
                    status: 400,
                    headers: Vec::new(),
                    body: b"body must be a decimal mailbox id".to_vec(),
                };
            };
            ctx.actor::<ComponentHostCapability>().send_detached(&DropComponent { mailbox_id: MailboxId(raw_id) });
            return HttpServerResponse { status: 200, headers: Vec::new(), body: b"dropping".to_vec() };
        }
        HttpServerResponse { status: 200, headers: Vec::new(), body: b"routed handler".to_vec() }
    }
}

/// Response-streaming handler reached through an **externally-registered
/// route** rather than the configured default `handler_mailbox` (ADR-0128
/// streaming × ADR-0131 route dispatch). It claims `/routed-stream` for its
/// own mailbox with a raw `RegisterRouteSelf` from `wire`, then behaves
/// exactly like [`StreamingHttpHandler`]: replies `HttpResponseStreamOpen`
/// and emits `STREAM_CHUNK_COUNT` credit-paced `"chunk-{i}\n"` chunks. The
/// e2e test points `HttpServerConfig.handler_mailbox` at a *different*
/// mailbox, so the default-handler dispatch cannot mask the route path — the
/// initial response-stream credit grant must reach this handler via the route
/// it registered, not the configured default.
///
/// Registered at `aether.component/aether.embedded:test.web_stream_routed`
/// after load.
pub struct RoutedStreamingHttpHandler {
    /// The stream this handler is feeding (ADR-0133), captured from the first
    /// credit mail. `None` until that first grant arms it.
    stream: Option<ResponseStream>,
    /// Index of the next chunk to emit.
    next_index: u32,
    /// Whether the terminator has been sent.
    ended: bool,
}

#[actor]
impl WasmActor for RoutedStreamingHttpHandler {
    const NAMESPACE: &'static str = "test.web_stream_routed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(RoutedStreamingHttpHandler { stream: None, next_index: 0, ended: false })
    }

    /// Claim `/routed-stream` for this actor's own mailbox through the raw
    /// reflexive route form (ADR-0131). Registering from `wire` is the
    /// common "route to me" case; `ctx` resolves the `aether.http.server`
    /// cap by type, so the send reads exactly as the `#[http::route]` macro's
    /// injected registration does.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
            prefix: "/routed-stream".to_string(),
            method: None,
            kind: <HttpServerRequest as Kind>::ID,
            shared: false,
        });
    }

    /// Open a streamed `200` response. Dispatched here through the registered
    /// route, not the default `handler_mailbox`.
    ///
    /// # Agent
    /// Not sent manually — the `aether.http.server` cap dispatches it on a
    /// request matching the `/routed-stream` route this actor claimed in
    /// `wire`.
    #[handler::single]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, _req: HttpServerRequest) -> HttpResponseStreamOpen {
        self.next_index = 0;
        self.ended = false;
        HttpResponseStreamOpen { status: 200, headers: Vec::new() }
    }

    /// Spend the granted credit exactly as [`StreamingHttpHandler`] does. On
    /// today's bug the initial grant never arrives for a route-dispatched
    /// stream, so this never fires and no chunk is emitted.
    ///
    /// # Agent
    /// Not sent manually — the cap sends one `HttpStreamCredit` per freed
    /// window slot.
    #[handler::manual]
    fn on_credit(&mut self, ctx: &mut WasmCtx<'_, Manual>, credit: HttpStreamCredit) {
        spend_credit(&mut self.stream, &mut self.next_index, &mut self.ended, ctx, &credit);
    }
}
