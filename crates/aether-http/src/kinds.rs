//! Wire kinds owned by the two HTTP capabilities (ADR-0121): the egress
//! client (`client.rs`) and the ingress server (`server.rs`). The
//! substrate core dispatches none of them, so they live with the
//! capabilities that own them rather than in `aether-kinds`. The module
//! is always-on and wasm-safe — the types depend only on
//! `aether_data::{Kind, Schema}`, serde, and `aether_actor`'s protocol and
//! typed-path vocabulary (all three unconditional dependencies), so the
//! `default-features = false` wasm consumers keep compiling.

use aether_actor::{HeldReply, ProtocolPath};
use core::fmt;
use serde::{Deserialize, Serialize};

// ADR-0043 substrate HTTP egress. One request kind + one reply
// kind on the `"aether.http"` sink, plus supporting `HttpMethod`,
// `HttpHeader`, and `HttpError` shapes. All structured
// (Strings, Vecs, Option<u32>).
//
// Reply correlation is the caller-minted `request_id` echoed on
// both `FetchResult` arms (ADR-0158 §6). The substrate dispatches
// fetches per-sender-bounded and concurrently, so two requests to
// the same `url` — a retry, a non-idempotent `POST` — would reply
// indistinguishably under the older `url`-echo correlation; the
// caller stamps a distinct `request_id` per request and matches the
// reply by it. The `url` stays on both arms as an informational echo
// (log / MCP-caller readability), demoted from correlation key.
// Request `body` is not echoed — correlation needs the identity of
// the request, not its contents, and a multi-MB upload should not
// round-trip its bytes.

/// HTTP method carried on `Fetch`. Enumerating at the schema
/// layer keeps `"get"` / `"GET"` / `"Get"` from disagreeing
/// across guests; the substrate maps each variant to its
/// canonical uppercase name when calling the HTTP backend.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
}

impl HttpMethod {
    /// The canonical uppercase verb for this method — the render
    /// counterpart of `parse_http_method` (the server runtime's
    /// str-to-variant table). The two are independent match tables owning
    /// the same seven spellings.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One HTTP header on a `Fetch` request or `FetchResult`
/// response. Expressed as a named-field struct because
/// `aether_data::Schema` has no blanket impl for tuples — if
/// that lands later the wire shape here is source-compatible
/// (same two fields in the same order).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

/// Structured failure reason for an HTTP request (ADR-0043 §1).
/// Typed variants cover the branches agents routinely need to
/// match on — `Timeout` → retry, `AllowlistDenied` → config
/// issue, `BodyTooLarge` → chunk the response, `Disabled` →
/// surface to the operator. `InvalidUrl` carries the offending
/// URL text; `AdapterError` is the catchall preserving backend-
/// specific detail (DNS failure, TLS handshake, connection
/// refused, etc.) as free-form text. `Closed` is the answer a caller
/// receives when the capability closes before its fetch answered.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    InvalidUrl(String),
    Timeout,
    BodyTooLarge,
    AllowlistDenied,
    Disabled,
    AdapterError(String),
    /// The capability closed before the fetch answered (ADR-0243 §1).
    Closed,
}

/// `aether.http.fetch` — request the substrate perform an HTTP
/// request and reply with the response. Mailed to the
/// `"aether.http"` sink; reply lands via `reply_mail` as
/// `FetchResult`.
///
/// `request_id` is a caller-minted correlation id echoed on both
/// `FetchResult` arms (ADR-0158 §6). The cap dispatches fetches
/// concurrently under a per-sender bound, so a caller firing two
/// requests to the same `url` matches each reply by its own
/// `request_id` rather than by the ambiguous echoed `url`.
///
/// `timeout_ms` overrides the chassis default
/// (`AETHER_HTTP_TIMEOUT_MS`, default 30000) when set; `None`
/// uses the default.
#[aether_data::kind(name = "aether.http.fetch")]
pub struct Fetch {
    pub request_id: u64,
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<HttpHeader>,
    #[serde(with = "aether_data::bytes")]
    pub body: Vec<u8>,
    pub timeout_ms: Option<u32>,
}

/// Reply to `Fetch`. Both arms echo the caller-minted `request_id`
/// (ADR-0158 §6) — the correlator a caller matches reply-to-request
/// by, unambiguous even for two concurrent requests to the same
/// `url` — plus the originating `url` as an informational echo (log
/// / MCP-caller readability), demoted from correlation key. Request
/// `body` is deliberately not echoed: correlation needs the identity
/// of the request, not its contents, and a multi-MB upload should
/// not round-trip. `Ok` carries the HTTP status, response headers,
/// and response body (bounded by `AETHER_HTTP_MAX_BODY_BYTES`,
/// default 16MB); `Err` carries an `HttpError` variant.
#[aether_data::kind(name = "aether.http.fetch_result")]
pub enum FetchResult {
    Ok {
        request_id: u64,
        url: String,
        status: u16,
        headers: Vec<HttpHeader>,
        #[serde(with = "aether_data::bytes")]
        body: Vec<u8>,
    },
    Err {
        request_id: u64,
        url: String,
        error: HttpError,
    },
}

impl HeldReply for FetchResult {
    fn unanswered() -> Self {
        Self::Err { request_id: 0, url: String::new(), error: HttpError::Closed }
    }
}

// ADR-0108 HTTP server kinds. Two public kinds shared by the server
// capability (#1760) and the handler component (#1762): an inbound
// request delivered to the handler, and an outbound response returned
// by the handler. Both reuse `HttpMethod` / `HttpHeader` from ADR-0043
// so the inbound vocabulary is symmetric with the client.

/// Inbound HTTP request delivered to a handler component by the server
/// capability (ADR-0108). `query` is always present — empty string when
/// the URL carries no query component. `body` is raw bytes so binary
/// uploads round-trip without loss. `peer_addr` is the connecting
/// peer's address in `SocketAddr` display form (`ip:port`, IPv6
/// bracketed), supplied by the cap for handler-side logging /
/// rate-limit / allowlisting (ADR-0108 §6).
#[aether_data::kind(name = "aether.http.server.request")]
pub struct HttpServerRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: String,
    pub headers: Vec<HttpHeader>,
    #[serde(with = "aether_data::bytes")]
    pub body: Vec<u8>,
    pub peer_addr: String,
}

/// Outbound HTTP response produced by a handler component and forwarded
/// to the waiting client by the server capability (ADR-0108). `status`
/// is the raw HTTP status code; `body` is raw bytes so binary responses
/// round-trip without loss.
#[aether_data::kind(name = "aether.http.server.response")]
pub struct HttpServerResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    #[serde(with = "aether_data::bytes")]
    pub body: Vec<u8>,
}

/// A router's one reply to an [`HttpServerRequest`] (ADR-0243 §8): the
/// three answers the server accepts, named as one kind so the
/// [`HttpRouter`] row declares its reply and a plain handler covers it.
///
/// - [`Response`](Self::Response) answers with a buffered
///   [`HttpServerResponse`] (ADR-0108), which is also how a router declines a
///   websocket upgrade.
/// - [`Stream`](Self::Stream) opens a streamed response
///   ([`HttpResponseStreamOpen`], ADR-0128).
/// - [`WebSocket`](Self::WebSocket) accepts an upgrade
///   ([`WebSocketAccept`], ADR-0129).
#[aether_data::kind(name = "aether.http.server.router_result")]
pub enum HttpRouterResult {
    Response(HttpServerResponse),
    Stream(HttpResponseStreamOpen),
    WebSocket(WebSocketAccept),
}

/// A router that closes while it still owes a reply answers `502`
/// (ADR-0243 §1), so the waiting client hears at once instead of at the
/// server's request timeout.
impl HeldReply for HttpRouterResult {
    fn unanswered() -> Self {
        Self::Response(HttpServerResponse {
            status: 502,
            headers: Vec::new(),
            body: b"router closed before answering".to_vec(),
        })
    }
}

/// The contract every route holder covers (ADR-0231 §2): one row,
/// [`HttpServerRequest`] answered with one [`HttpRouterResult`]. Both
/// registration forms prove it, [`RegisterRoute`] through its `handler` path
/// and [`RegisterRouteSelf`] through a cast of the stamped sender, and every
/// route holds a reference to it, so the server delivers a request only to an
/// actor that takes one.
///
/// A handler that returns `HttpRouterResult` covers the row, and so does one
/// that returns `Pending<HttpRouterResult>` and answers later through its held
/// reply (ADR-0243), as a hand-written handler that forwards to a peer does.
/// An unchecked handler does not cover it.
#[aether_actor::protocol]
pub trait HttpRouter {
    fn request(mail: HttpServerRequest) -> HttpRouterResult;
}

/// What the server sends a route holder whose request it answers with a
/// streamed response or an upgraded websocket: the [`HttpStreamCredit`]
/// grants that pace its outbound chunks or messages (ADR-0128, ADR-0129 §3).
/// The server casts every route holder to it once, at registration (ADR-0231
/// §4), and answers `502` to a [`HttpRouterResult::Stream`] or
/// [`HttpRouterResult::WebSocket`] reply from a holder that does not cover it.
///
/// A `#[handler::tell]` taking `HttpStreamCredit` covers the row.
#[aether_actor::protocol]
pub trait StreamCreditRouter {
    fn credit(mail: HttpStreamCredit);
}

/// What the server sends a route holder that takes a streamed upload
/// (ADR-0128): the stream's head, its body pieces, and its terminator, which
/// the holder answers with the one buffered response. The server casts every
/// route holder to it once, at registration (ADR-0231 §4), and streams a
/// request body only to a holder that covers every row; any other holder gets
/// the buffered [`HttpServerRequest`].
///
/// `#[handler::tell]` handlers taking `HttpRequestStreamOpen` and
/// `HttpRequestChunk` cover the first two rows. A handler taking
/// `HttpRequestStreamEnd` that returns `HttpServerResponse`, or
/// `Pending<HttpServerResponse>` and answers later through its held reply
/// (ADR-0243), covers the third.
#[aether_actor::protocol]
pub trait RequestStreamRouter {
    fn open(mail: HttpRequestStreamOpen);
    fn chunk(mail: HttpRequestChunk);
    fn end(mail: HttpRequestStreamEnd) -> HttpServerResponse;
}

/// What the server sends a route holder that holds an upgraded websocket
/// (ADR-0129): each inbound message and the peer's close. A websocket is paced
/// by [`HttpStreamCredit`] too, so a holder that accepts one covers
/// [`StreamCreditRouter`] as well. The server casts every route holder to it
/// once, at registration (ADR-0231 §4), and answers `502` in place of the
/// `101` to a [`HttpRouterResult::WebSocket`] reply from a holder that does
/// not cover both.
///
/// `#[handler::tell]` handlers taking `WebSocketMessage` and
/// `WebSocketClose` cover the rows.
#[aether_actor::protocol]
pub trait WebSocketRouter {
    fn message(mail: WebSocketMessage);
    fn close(mail: WebSocketClose);
}

// ADR-0128 HTTP server response streaming. A handler opts into streaming by
// replying `HttpRouterResult::Stream` instead of `HttpRouterResult::Response`, emits its
// body across many `HttpResponseChunk` mails paced by the cap's
// `HttpStreamCredit` grants, and terminates with `HttpResponseStreamEnd`.
//
// Correlation carries an explicit `stream_id` on the chunk / end / credit
// kinds rather than riding the transport-envelope correlation the buffered
// reply uses: the guest reply handle is one-shot, so a handler cannot re-echo
// the request correlation across many chunks (ADR-0128 reconciles §2's
// "keyed by correlation id" wording with this payload field). The cap sets
// `stream_id` to the request's dispatch correlation id `C` — the same key its
// in-flight table already holds — and the handler learns `C` from the first
// `HttpStreamCredit`. The `HttpResponseStreamOpen` reply still rides the
// one-shot correlation-echoed reply path, so the open handshake keys on `C`
// directly.

/// `aether.http.server.response_stream_open` — a handler's first reply on a
/// streamed response (ADR-0128), carried as the [`HttpRouterResult::Stream`]
/// variant. Declares the status line and headers; the cap writes the response
/// head with `Transfer-Encoding: chunked` (no `Content-Length`) and begins the
/// stream. The reply is correlation-echoed, so the cap keys the new stream by
/// the request's in-flight correlation id.
#[aether_data::kind(name = "aether.http.server.response_stream_open")]
pub struct HttpResponseStreamOpen {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
}

/// `aether.http.server.stream_credit` — the cap → handler backpressure grant
/// (ADR-0128). Grants permission to send up to `credit` more
/// [`HttpResponseChunk`] mails on the stream named by `stream_id`. The handler
/// learns its `stream_id` from the first credit mail (the cap sets it to the
/// request's dispatch correlation id) and pauses when its accumulated credit
/// reaches zero.
#[aether_data::kind(name = "aether.http.server.stream_credit")]
pub struct HttpStreamCredit {
    pub stream_id: u64,
    pub credit: u32,
}

/// `aether.http.server.response_chunk` — handler → cap, one body piece on the
/// stream named by `stream_id` (ADR-0128). Consumes one unit of credit; the
/// cap frames it as one chunked-transfer chunk to the peer. `body` is raw
/// bytes so binary downloads stream without loss.
#[aether_data::kind(name = "aether.http.server.response_chunk")]
pub struct HttpResponseChunk {
    pub stream_id: u64,
    #[serde(with = "aether_data::bytes")]
    pub body: Vec<u8>,
}

/// `aether.http.server.response_stream_end` — handler → cap terminator on the
/// stream named by `stream_id` (ADR-0128). The cap writes the terminating
/// zero-length chunk and closes the connection.
#[aether_data::kind(name = "aether.http.server.response_stream_end")]
pub struct HttpResponseStreamEnd {
    pub stream_id: u64,
}

// ADR-0128 HTTP server request streaming. The request-side mirror of the
// response-streaming vocabulary above, with the credit direction inverted:
// here the peer is the producer, so the cap streams the inbound body to a
// streaming handler across many `HttpRequestChunk` mails and the *handler*
// grants credit back to the cap with `HttpRequestCredit`. A handler opts in
// structurally — by covering `RequestStreamRouter` — so the cap reads the
// decision off the cast it made when the route was registered rather than
// from a per-request reply (a request handler cannot reply before it receives
// the request).
//
// Each kind carries an explicit `stream_id` for the same reason the
// response-side chunk / credit kinds do: the mid-stream mails are per-chunk
// causal chains (no stream-long settlement hold), so envelope correlation
// cannot tie a handler's `HttpRequestCredit` back to the connection it paces.
// The cap mints a `stream_id` when it opens the stream and stamps it on the
// `HttpRequestStreamOpen`; the handler echoes it on every `HttpRequestCredit`,
// and the cap demultiplexes concurrent uploads by it. The handler's final
// buffered `HttpServerResponse` rides the `HttpRequestStreamEnd` dispatch's
// envelope correlation, so it needs no `stream_id`.

/// `aether.http.server.request_stream_open` — the cap's first mail to a
/// streaming handler when a streamed request begins (ADR-0128). It is
/// [`HttpServerRequest`] minus the body: the request head, with the body to
/// follow as [`HttpRequestChunk`] mails on the stream named by `stream_id`.
/// The handler learns its `stream_id` here and stamps it on every
/// [`HttpRequestCredit`] it sends back.
#[aether_data::kind(name = "aether.http.server.request_stream_open")]
pub struct HttpRequestStreamOpen {
    pub stream_id: u64,
    pub method: HttpMethod,
    pub path: String,
    pub query: String,
    pub headers: Vec<HttpHeader>,
}

/// `aether.http.server.request_chunk` — cap → handler, one inbound body piece
/// on the stream named by `stream_id` (ADR-0128). Each consumes one unit of
/// the cap's send window; the handler replenishes by mailing
/// [`HttpRequestCredit`] as it drains chunks. `body` is raw bytes so binary
/// uploads stream without loss.
#[aether_data::kind(name = "aether.http.server.request_chunk")]
pub struct HttpRequestChunk {
    pub stream_id: u64,
    #[serde(with = "aether_data::bytes")]
    pub body: Vec<u8>,
}

/// `aether.http.server.request_stream_end` — cap → handler terminator on the
/// stream named by `stream_id` (ADR-0128): the peer finished the body (socket
/// EOF for a `Content-Length` body once fully read, or the zero-length
/// terminating chunk for a chunked body). The handler replies its buffered
/// [`HttpServerResponse`] to *this* mail — the cap keys the response on the
/// terminator's envelope correlation — so a streamed upload still answers with
/// one ordinary response.
#[aether_data::kind(name = "aether.http.server.request_stream_end")]
pub struct HttpRequestStreamEnd {
    pub stream_id: u64,
}

/// `aether.http.server.request_credit` — the handler → cap backpressure grant
/// (ADR-0128), the inverse of [`HttpStreamCredit`]. Tells the cap it may
/// deliver up to `credit` more [`HttpRequestChunk`] mails on the stream named
/// by `stream_id`. A full window parks the cap's per-connection socket reader,
/// at which point the unread bytes back up into the kernel receive buffer and
/// TCP backpressure blocks the peer's send — so a fast peer cannot outrun a
/// slow handler unboundedly.
#[aether_data::kind(name = "aether.http.server.request_credit")]
pub struct HttpRequestCredit {
    pub stream_id: u64,
    pub credit: u32,
}

// ADR-0129 HTTP server websocket upgrade, amended by ADR-0132. Three kinds,
// reusing `HttpHeader` and ADR-0128's `HttpStreamCredit`. An inbound request
// carrying `Upgrade: websocket` dispatches to the handler as an ordinary
// `HttpServerRequest`; the handler replies `HttpRouterResult::WebSocket` to
// accept (the websocket analog of `HttpRouterResult::Stream`) or an ordinary
// `HttpRouterResult::Response` to decline. On accept the connection carries
// `WebSocketMessage`s both directions — cap → handler on inbound (a fresh
// causal root per message), handler → cap on outbound (framed under the
// ADR-0128 credit window). `WebSocketClose` is the close handshake, both
// directions. Every data-phase kind carries the connection's `stream_id`
// (ADR-0132): the cap mints it at accept, the handler learns it from the
// initial `HttpStreamCredit` grant, and outbound mail names its target
// connection with it — routing that holds from any causal chain, so a
// handler can push unprompted. Ping / pong are cap-owned and never surface
// as kinds.

/// `aether.http.server.websocket.accept` — the handler's opt-in reply to an
/// upgrade request (ADR-0129), carried as the [`HttpRouterResult::WebSocket`]
/// variant, the websocket analog of [`HttpRouterResult::Stream`]. Declares an
/// optional negotiated `subprotocol` (echoed as `Sec-WebSocket-Protocol`) and
/// any extra `101` response `headers`; the cap supplies `Upgrade` /
/// `Connection` / `Sec-WebSocket-Accept` itself. The reply is
/// correlation-echoed, so the cap keys the accept on the request's in-flight
/// correlation id.
#[aether_data::kind(name = "aether.http.server.websocket.accept")]
pub struct WebSocketAccept {
    pub subprotocol: Option<String>,
    pub headers: Vec<HttpHeader>,
}

/// `aether.http.server.websocket.message` — one complete, de-fragmented
/// application message, both directions (ADR-0129 / ADR-0132). Cap → handler
/// on inbound (dispatched as a fresh causal root that settles as the handler
/// finishes it); handler → cap on outbound (serialized to an RFC 6455 frame
/// and framed to the peer under the ADR-0128 credit window). `binary` selects
/// the RFC 6455 opcode (`true` = binary `0x2`, `false` = text `0x1`); `data`
/// is the reassembled payload, raw bytes so binary messages round-trip
/// without loss.
///
/// `stream_id` names the connection (ADR-0132). Inbound, the cap stamps the
/// upgraded connection's stream id — the same id the handler's
/// [`HttpStreamCredit`] grants carry — so a handler serving several sockets
/// tells their messages apart. Outbound, the handler addresses the target
/// connection with it and the cap resolves the socket through its stream
/// table, exactly as it routes an [`HttpResponseChunk`]; the send routes
/// identically from any causal chain, so a handler can push with no inbound
/// message in flight. An unknown or torn-down `stream_id` drops the message.
#[aether_data::kind(name = "aether.http.server.websocket.message")]
pub struct WebSocketMessage {
    pub stream_id: u64,
    pub binary: bool,
    #[serde(with = "aether_data::bytes")]
    pub data: Vec<u8>,
}

/// `aether.http.server.websocket.close` — the close handshake, both directions
/// (ADR-0129 / ADR-0132). Handler → cap initiates a close; cap → handler
/// reports a peer-initiated close. The cap writes / echoes the RFC 6455 close
/// frame and tears the connection down. `code` is the RFC 6455 close status
/// code (`1000` = normal); `reason` is the optional UTF-8 reason phrase.
/// `stream_id` names the connection like [`WebSocketMessage`]'s (ADR-0132),
/// both directions.
#[aether_data::kind(name = "aether.http.server.websocket.close")]
pub struct WebSocketClose {
    pub stream_id: u64,
    pub code: u16,
    pub reason: String,
}

// ADR-0130 route-registration kinds. Mirrors the `aether.window`
// subscribe family: `_self` variants resolve the registrant from the
// inbound envelope's host-stamped `Source` (forgery-proof, in-process
// by construction); the explicit variants serve external callers and
// name the handler by its canonical path, proven against the engine.
// Either way the holder covers `HttpRouter`, so every route dispatches as
// `aether.http.server.request` and neither form names a kind.

/// `aether.http.server.register_route` — claim a path-prefix route for the
/// actor at `handler`. `prefix` is segment-boundary matched (`/api` matches
/// `/api` and `/api/…`, never `/apiary`; `/` is the catch-all; a
/// trailing slash is normalized off at registration). `method` filters
/// the route to one HTTP method; `None` accepts every method. Among
/// matching routes the longest prefix wins, and a method-specific
/// route beats a method-agnostic one at equal prefix. A `(prefix,
/// method)` key already claimed by a *different* handler is answered
/// `Err`; the same handler re-claiming its own key is an idempotent
/// `Ok`. Reply: `RegisterRouteResult`.
///
/// `handler` is the canonical path of an actor covering [`HttpRouter`]
/// (ADR-0231 §3): in code `ActorPath::<R>::root().narrow::<HttpRouter>()`,
/// which compiles only when `R` takes `aether.http.server.request` and
/// replies `HttpRouterResult`; over MCP the `path` a component load returns.
/// A path whose handler has closed decodes and gets `Err` naming it; a path
/// no such route has stood at is refused at decode with a warn and gets no
/// reply.
///
/// `shared` (ADR-0136) opts the registration into the key's member
/// *set*: N handler instances that all register `shared: true` jointly serve
/// the route, each request picked round-robin across live members. `false` is
/// the exclusive claim described above. Mixing the two on one key is a
/// conflict `Err` — spreading is something instances opt into together, never
/// an accident.
#[aether_data::kind(name = "aether.http.server.register_route", no_serde)]
pub struct RegisterRoute {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub handler: ProtocolPath<HttpRouter>,
    pub shared: bool,
}

/// `aether.http.server.register_route_self` — reflexive counterpart of
/// [`RegisterRoute`]: claim the route for the *sending* actor, with no
/// explicit `handler` path. The cap resolves the registrant from the
/// inbound envelope's host-stamped `Source` (ADR-0083), so the
/// registrant cannot be forged and the op is gated to in-process
/// actors by construction — an external session or another engine gets
/// an `Err` reply, pushing it onto the named [`RegisterRoute`] form.
/// This is the common "route to me" case, sent from `wire`, and the form
/// `#[http::router]` emits. The cap casts the sender to [`HttpRouter`], and
/// a sender whose rows do not cover it gets an `Err` naming the missing row.
/// Reply: `RegisterRouteResult`.
///
/// `shared` (ADR-0136) opts into the key's member set exactly as on
/// [`RegisterRoute`]: instanced handlers that all register `shared:
/// true` jointly serve the route round-robin; `false` is the exclusive
/// claim.
#[aether_data::kind(name = "aether.http.server.register_route_self")]
pub struct RegisterRouteSelf {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub shared: bool,
}

/// `aether.http.server.unregister_route` — release the `(prefix,
/// method)` route held by the actor at `handler`. Idempotent: releasing a
/// route that isn't held is still `Ok`, and so is a path no live route stands
/// at — a departed holder's routes already went with its `MonitorNotice`.
/// Reply: `RegisterRouteResult`.
///
/// `handler` names identity only, so it is a plain [`ErasedActorPath`]
/// (ADR-0231 §3, proven with `resolve_path`) rather than an [`HttpRouter`]
/// path: the server never sends to a holder it is releasing, so a release
/// needs the holder's identity and nothing its rows promise. `resolve_path`
/// fills a short path's holes, so an ADR-0166 short path (`root/:key`)
/// releases as well as the canonical spelling does.
///
/// [`ErasedActorPath`]: aether_data::ErasedActorPath
#[aether_data::kind(name = "aether.http.server.unregister_route")]
pub struct UnregisterRoute {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub handler: aether_data::ErasedActorPath,
}

/// `aether.http.server.unregister_route_self` — reflexive counterpart
/// of [`UnregisterRoute`]: release the *sending* actor's `(prefix,
/// method)` route, resolved from the host-stamped `Source` like
/// [`RegisterRouteSelf`]. Idempotent. Reply: `RegisterRouteResult`.
#[aether_data::kind(name = "aether.http.server.unregister_route_self")]
pub struct UnregisterRouteSelf {
    pub prefix: String,
    pub method: Option<HttpMethod>,
}

/// Reply to the route registration / unregistration kinds (ADR-0130).
/// Failure modes: an invalid prefix (must start with `/`), a `handler` path
/// whose actor has closed, a `(prefix, method)` key already claimed by
/// another handler, or a `_self` op from a sender with no local mailbox.
/// A `handler` path no route has stood at, or whose route does not cover
/// [`HttpRouter`], never reaches the receipt at all: the decode refuses the mail, which is logged
/// at warn and gets no reply of any kind. A `_self` registrant that does not
/// cover it is answered `Err`.
#[aether_data::kind(name = "aether.http.server.register_route_result")]
pub enum RegisterRouteResult {
    Ok,
    Err { error: String },
}

/// `aether.http.server.inbound_ready` — accept / reader sidecar →
/// `HttpServerCapability` dispatcher wake (ADR-0108, issue 1760).
/// The HTTP-server analog of `RpcInboundReady`: the sidecar pushes
/// the live work (an accepted `TcpStream`, a parsed request, a close
/// reason) over the cap's internal mpsc and fires this empty-payload
/// mail at the cap's own mailbox so the dispatcher handler drains the
/// queue. A `TcpStream` isn't wire-shaped and a request body may be
/// large, so the mail is only the wakeup signal.
#[aether_data::kind(name = "aether.http.server.inbound_ready", default)]
pub struct HttpInboundReady {}
