//! Wire kinds owned by the two HTTP capabilities (ADR-0121): the egress
//! client (`client.rs`) and the ingress server (`server.rs`). The
//! substrate core dispatches none of them, so they live with the
//! capabilities that own them rather than in `aether-kinds`. The module
//! is always-on and wasm-safe — the types depend only on
//! `aether_data::{Kind, Schema}` and serde, so the
//! `default-features = false` wasm consumers keep compiling.

use serde::{Deserialize, Serialize};
use std::fmt;

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
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
/// refused, etc.) as free-form text.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    InvalidUrl(String),
    Timeout,
    BodyTooLarge,
    AllowlistDenied,
    Disabled,
    AdapterError(String),
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
/// `request_id` rather than by the ambiguous echoed `url`. Mirrors
/// `MessagesSend.request_id` in `aether-anthropic`.
///
/// `timeout_ms` overrides the chassis default
/// (`AETHER_HTTP_TIMEOUT_MS`, default 30000) when set; `None`
/// uses the default.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.fetch")]
pub struct Fetch {
    pub request_id: u64,
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<HttpHeader>,
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.fetch_result")]
pub enum FetchResult {
    Ok { request_id: u64, url: String, status: u16, headers: Vec<HttpHeader>, body: Vec<u8> },
    Err { request_id: u64, url: String, error: HttpError },
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.request")]
pub struct HttpServerRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
    pub peer_addr: String,
}

/// Outbound HTTP response produced by a handler component and forwarded
/// to the waiting client by the server capability (ADR-0108). `status`
/// is the raw HTTP status code; `body` is raw bytes so binary responses
/// round-trip without loss.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.response")]
pub struct HttpServerResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

// ADR-0128 HTTP server response streaming. A handler opts into streaming by
// replying `HttpResponseStreamOpen` instead of `HttpServerResponse`, emits its
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
/// streamed response (ADR-0128). Declares the status line and headers; the
/// cap writes the response head with `Transfer-Encoding: chunked` (no
/// `Content-Length`) and begins the stream. Replied like [`HttpServerResponse`]
/// (correlation-echoed), so the cap keys the new stream by the request's
/// in-flight correlation id.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.response_stream_open")]
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.stream_credit")]
pub struct HttpStreamCredit {
    pub stream_id: u64,
    pub credit: u32,
}

/// `aether.http.server.response_chunk` — handler → cap, one body piece on the
/// stream named by `stream_id` (ADR-0128). Consumes one unit of credit; the
/// cap frames it as one chunked-transfer chunk to the peer. `body` is raw
/// bytes so binary downloads stream without loss.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.response_chunk")]
pub struct HttpResponseChunk {
    pub stream_id: u64,
    pub body: Vec<u8>,
}

/// `aether.http.server.response_stream_end` — handler → cap terminator on the
/// stream named by `stream_id` (ADR-0128). The cap writes the terminating
/// zero-length chunk and closes the connection.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.response_stream_end")]
pub struct HttpResponseStreamEnd {
    pub stream_id: u64,
}

// ADR-0128 HTTP server request streaming. The request-side mirror of the
// response-streaming vocabulary above, with the credit direction inverted:
// here the peer is the producer, so the cap streams the inbound body to a
// streaming handler across many `HttpRequestChunk` mails and the *handler*
// grants credit back to the cap with `HttpRequestCredit`. A handler opts in
// structurally — by declaring it accepts `HttpRequestStreamOpen` — so the cap
// reads the decision off the handler's accept-set at dispatch time rather than
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.request_stream_open")]
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.request_chunk")]
pub struct HttpRequestChunk {
    pub stream_id: u64,
    pub body: Vec<u8>,
}

/// `aether.http.server.request_stream_end` — cap → handler terminator on the
/// stream named by `stream_id` (ADR-0128): the peer finished the body (socket
/// EOF for a `Content-Length` body once fully read, or the zero-length
/// terminating chunk for a chunked body). The handler replies its buffered
/// [`HttpServerResponse`] to *this* mail — the cap keys the response on the
/// terminator's envelope correlation — so a streamed upload still answers with
/// one ordinary response.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.request_stream_end")]
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.request_credit")]
pub struct HttpRequestCredit {
    pub stream_id: u64,
    pub credit: u32,
}

// ADR-0129 HTTP server websocket upgrade, amended by ADR-0132. Three kinds,
// reusing `HttpHeader` and ADR-0128's `HttpStreamCredit`. An inbound request
// carrying `Upgrade: websocket` dispatches to the handler as an ordinary
// `HttpServerRequest`; the handler replies `WebSocketAccept` to accept (the
// websocket analog of `HttpResponseStreamOpen`) or an ordinary
// `HttpServerResponse` to decline. On accept the connection carries
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
/// upgrade request (ADR-0129), the websocket analog of
/// [`HttpResponseStreamOpen`]. Declares an optional negotiated `subprotocol`
/// (echoed as `Sec-WebSocket-Protocol`) and any extra `101` response
/// `headers`; the cap supplies `Upgrade` / `Connection` /
/// `Sec-WebSocket-Accept` itself. Replied like [`HttpServerResponse`]
/// (correlation-echoed), so the cap keys the accept on the request's in-flight
/// correlation id.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.websocket.accept")]
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.websocket.message")]
pub struct WebSocketMessage {
    pub stream_id: u64,
    pub binary: bool,
    pub data: Vec<u8>,
}

/// `aether.http.server.websocket.close` — the close handshake, both directions
/// (ADR-0129 / ADR-0132). Handler → cap initiates a close; cap → handler
/// reports a peer-initiated close. The cap writes / echoes the RFC 6455 close
/// frame and tears the connection down. `code` is the RFC 6455 close status
/// code (`1000` = normal); `reason` is the optional UTF-8 reason phrase.
/// `stream_id` names the connection like [`WebSocketMessage`]'s (ADR-0132),
/// both directions.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.websocket.close")]
pub struct WebSocketClose {
    pub stream_id: u64,
    pub code: u16,
    pub reason: String,
}

// ADR-0130 route-registration kinds. Mirrors the `aether.input`
// subscribe family: `_self` variants resolve the registrant from the
// inbound envelope's host-stamped `Source` (forgery-proof, in-process
// by construction); the explicit-`mailbox` variants serve external
// callers and are validated against the registry. A route carries the
// `KindId` its requests dispatch as — the cap stamps that kind onto
// the request-shaped payload, so the registered kind's schema must
// decode `aether.http.server.request`'s byte layout
// (`aether.http.server.request` itself is the generic choice).

/// `aether.http.server.register_route` — claim a path-prefix route for
/// `mailbox`. `prefix` is segment-boundary matched (`/api` matches
/// `/api` and `/api/…`, never `/apiary`; `/` is the catch-all; a
/// trailing slash is normalized off at registration). `method` filters
/// the route to one HTTP method; `None` accepts every method. Among
/// matching routes the longest prefix wins, and a method-specific
/// route beats a method-agnostic one at equal prefix. A `(prefix,
/// method)` key already claimed by a *different* mailbox is answered
/// `Err`; the same mailbox re-claiming its own key is an idempotent
/// `Ok` (and updates `kind`). Reply: `RegisterRouteResult`.
///
/// `shared` (ADR-0136) opts the registration into the key's member
/// *set*: N handler instances that all register `shared: true` with the
/// same dispatch `kind` jointly serve the route, each request picked
/// round-robin across live members. `false` is the exclusive claim
/// described above. Mixing the two on one key, or joining with a
/// different `kind`, is a conflict `Err` — spreading is something
/// instances opt into together, never an accident.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.register_route")]
pub struct RegisterRoute {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub kind: aether_data::KindId,
    pub mailbox: aether_data::MailboxId,
    pub shared: bool,
}

/// `aether.http.server.register_route_self` — reflexive counterpart of
/// [`RegisterRoute`]: claim the route for the *sending* actor, with no
/// explicit `mailbox` field. The cap resolves the registrant from the
/// inbound envelope's host-stamped `Source` (ADR-0083), so the
/// registrant cannot be forged and the op is gated to in-process
/// actors by construction — an external session or another engine gets
/// an `Err` reply, pushing it onto the named [`RegisterRoute`] form.
/// This is the common "route to me" case, sent from `wire`. Reply:
/// `RegisterRouteResult`.
///
/// `shared` (ADR-0136) opts into the key's member set exactly as on
/// [`RegisterRoute`]: instanced handlers that all register `shared:
/// true` jointly serve the route round-robin; `false` is the exclusive
/// claim.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.register_route_self")]
pub struct RegisterRouteSelf {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub kind: aether_data::KindId,
    pub shared: bool,
}

/// `aether.http.server.unregister_route` — release the `(prefix,
/// method)` route held by `mailbox`. Idempotent: releasing a route
/// that isn't held is still `Ok`. Reply: `RegisterRouteResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.unregister_route")]
pub struct UnregisterRoute {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub mailbox: aether_data::MailboxId,
}

/// `aether.http.server.unregister_route_self` — reflexive counterpart
/// of [`UnregisterRoute`]: release the *sending* actor's `(prefix,
/// method)` route, resolved from the host-stamped `Source` like
/// [`RegisterRouteSelf`]. Idempotent. Reply: `RegisterRouteResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.unregister_route_self")]
pub struct UnregisterRouteSelf {
    pub prefix: String,
    pub method: Option<HttpMethod>,
}

/// Reply to the route registration / unregistration kinds (ADR-0130).
/// Failure modes: an invalid prefix (must start with `/`), an unknown
/// or dropped registrant mailbox, a `(prefix, method)` key already
/// claimed by another mailbox, or a `_self` op from a sender with no
/// local mailbox.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.register_route_result")]
pub enum RegisterRouteResult {
    Ok,
    Err { error: String },
}

/// `aether.http.server.unregister_routes_all` — release every route
/// held by `mailbox` in one shot. The externally sendable bulk form;
/// drop-time cleanup rides the ADR-0079 vacate/close `MonitorNotice`
/// instead, so the route table stops dispatching at a dropped
/// trampoline without anyone mailing this. Idempotent: a mailbox
/// holding no routes is a no-op. Fire-and-forget; no reply.
/// Cast-shape (Pod) — one `MailboxId`, fixed size.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.http.server.unregister_routes_all")]
pub struct UnregisterRoutesAll {
    pub mailbox: aether_data::MailboxId,
}

/// `aether.http.server.inbound_ready` — accept / reader sidecar →
/// `HttpServerCapability` dispatcher wake (ADR-0108, issue 1760).
/// The HTTP-server analog of `RpcInboundReady`: the sidecar pushes
/// the live work (an accepted `TcpStream`, a parsed request, a close
/// reason) over the cap's internal mpsc and fires this empty-payload
/// mail at the cap's own mailbox so the dispatcher handler drains the
/// queue. A `TcpStream` isn't wire-shaped and a request body may be
/// large, so the mail is only the wakeup signal.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.http.server.inbound_ready")]
pub struct HttpInboundReady {}
