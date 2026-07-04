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

/// Per-connection identifier, monotonic within this cap. Distinct from
/// the OS-level peer addr (one peer may reconnect; ids stay unique for
/// the cap's lifetime).
pub type ConnId = u64;

/// Header-array size for the inbound parse (doubles as the header-count
/// cap: a request with more headers is answered `431`). ADR-0108 §6.
const MAX_HEADER_COUNT: usize = 64;

/// How the request frames its body, decided from the head (ADR-0108 §4 /
/// ADR-0128). Drives both the reader's body read and the dispatcher's
/// buffered-vs-streamed-vs-reject decision.
#[derive(Copy, Clone)]
pub enum BodyFraming {
    /// A `Content-Length`-delimited body of this many bytes (0 = no body),
    /// with no `Transfer-Encoding`.
    Length(usize),
    /// A lone `Transfer-Encoding: chunked` body of unknown length — streamable
    /// (a streaming handler decodes it incrementally), un-bufferable (a
    /// buffered handler is answered `411`).
    Chunked,
    /// A framing the cap refuses `411`: `Content-Length` and
    /// `Transfer-Encoding` together (request smuggling), or a non-`chunked`
    /// transfer coding.
    Invalid,
}

/// The request head a reader hands the dispatcher for the streaming decision
/// (ADR-0128), before it reads the body. The method stays a raw `String`; the
/// dispatcher maps it to [`HttpMethod`] and answers `501` for a non-enumerated
/// verb.
pub struct ParsedHead {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<HttpHeader>,
    pub framing: BodyFraming,
    pub keep_alive: bool,
}

/// The dispatcher's decision, sent back to a parked reader over the
/// per-connection control channel (ADR-0128). A rejected request is handled by
/// the dispatcher writing the canned status and closing the connection —
/// dropping the control sender, which surfaces at the reader as a disconnect —
/// so there is no reject variant here.
pub enum ReaderControl {
    /// Read the `Content-Length` body into one [`InboundEvent::RequestParsed`]
    /// (today's buffered path).
    Buffered,
    /// Stream the body incrementally, seeded with `credit` initial send-window
    /// units (one per [`InboundEvent::RequestBodyChunk`]).
    Stream { credit: u32 },
    /// Replenish the streaming reader's send window by `credit` — the handler
    /// drained chunks and granted more.
    Credit { credit: u32 },
    /// Keep-alive resume: the response was written; read the next request on
    /// the same socket.
    Resume,
    /// Websocket upgrade accepted (ADR-0129): the `101` was written and the
    /// connection is now in websocket mode. The parked reader leaves the
    /// HTTP request lifecycle and enters the RFC 6455 frame loop, reading
    /// under the websocket idle deadline (not `request_timeout`), carrying
    /// forward any bytes it over-read past the handshake head.
    Upgrade,
}

/// One parsed inbound HTTP/1.1 request the reader hands to the
/// dispatcher. The method stays a raw `String` here; the dispatcher
/// maps it to [`HttpMethod`] and answers `501` for a non-enumerated
/// verb before any dispatch.
pub struct ParsedRequest {
    method: String,
    path: String,
    query: String,
    headers: Vec<HttpHeader>,
    body: Vec<u8>,
    /// Whether this request wants the connection kept alive after its
    /// response (HTTP/1.1 default, or HTTP/1.0 with `Connection:
    /// keep-alive`), computed by [`request_keeps_alive`]. Carried to the
    /// dispatcher so it renders `Connection: keep-alive` vs `close` and
    /// either resumes the reader or closes the socket.
    keep_alive: bool,
}

/// Internal event the accept / reader sidecar threads push to the cap
/// dispatcher via an mpsc. The matching wake-mail kind is
/// [`HttpInboundReady`] (empty payload) — `on_inbound_ready` drains the
/// channel and acts per item.
pub enum InboundEvent {
    /// The accept thread took a new connection.
    PeerAccepted { stream: TcpStream, peer: SocketAddr },
    /// A reader parsed a request head and needs the dispatcher's streaming
    /// decision before it reads the body (ADR-0128): the dispatcher resolves
    /// the handler, checks its accept-set, and replies down the connection's
    /// control channel with either [`ReaderControl::Buffered`] (read the body
    /// into one [`Self::RequestParsed`]) or [`ReaderControl::Stream`] (deliver
    /// it incrementally), or rejects the request and closes.
    RequestHeadParsed { conn_id: ConnId, head: ParsedHead },
    /// A reader parsed a complete, size-bounded request (buffered path).
    RequestParsed {
        conn_id: ConnId,
        request: ParsedRequest,
    },
    /// A streaming reader delivered one inbound body piece (ADR-0128); the
    /// dispatcher forwards it to the handler as an [`HttpRequestChunk`] on the
    /// connection's active stream.
    RequestBodyChunk { conn_id: ConnId, body: Vec<u8> },
    /// A streaming reader finished the request body (ADR-0128): the
    /// `Content-Length` count reached zero, or the chunked terminator arrived.
    /// The dispatcher sends the handler an [`HttpRequestStreamEnd`] and awaits
    /// its buffered response on that mail's correlation.
    RequestBodyEnd { conn_id: ConnId },
    /// A reader hit a trust cap or a parse error before any dispatch;
    /// the dispatcher writes the canned status response and closes.
    RequestRejected {
        conn_id: ConnId,
        status: u16,
        message: &'static str,
    },
    /// A reader saw EOF / a read error / a slow-loris timeout; the
    /// dispatcher tears the connection down.
    ReaderClosed { conn_id: ConnId, reason: String },
    /// The handler didn't reply within `request_timeout`; the
    /// dispatcher writes `504` if the request is still in-flight.
    RequestTimedOut { conn_id: ConnId },
    /// A response-stream writer thread drained one chunk to the socket,
    /// freeing a window slot (ADR-0128) — the dispatcher replenishes the
    /// handler's credit.
    StreamSlotFreed { stream_id: u64 },
    /// A response-stream writer thread finished — the terminating chunk was
    /// written, the peer disconnected, an idle-write deadline fired, or a
    /// socket error occurred — so the dispatcher tears the stream +
    /// connection down (ADR-0128).
    StreamFinished { stream_id: u64 },
    /// A websocket reader reassembled one complete application message
    /// (ADR-0129); the dispatcher delivers it to the handler as a
    /// [`WebSocketMessage`] on its own fresh causal root.
    WebSocketMessage {
        conn_id: ConnId,
        binary: bool,
        data: Vec<u8>,
    },
    /// A websocket reader received a ping frame (ADR-0129); the dispatcher
    /// answers it with a pong on the writer thread, transparently — the ping
    /// never reaches the handler. Carries the ping payload the pong echoes.
    WebSocketPing { conn_id: ConnId, payload: Vec<u8> },
    /// A websocket reader received a peer close frame, or hit a protocol error
    /// / oversize frame / idle timeout on the upgraded socket (ADR-0129). The
    /// dispatcher reports a [`WebSocketClose`] to the handler, echoes the close
    /// frame on the writer, and tears the connection down.
    WebSocketClose {
        conn_id: ConnId,
        code: u16,
        reason: String,
    },
}

/// One frame handed to a per-connection response-stream writer thread over
/// the bounded hand-off channel (ADR-0128). `Chunk` frames one body piece as
/// chunked transfer-encoding; `End` writes the terminating zero-length chunk.
enum WriterMsg {
    Chunk(Vec<u8>),
    End,
    /// A pre-serialized websocket data frame (ADR-0129), written verbatim.
    /// Consumes one credit like [`Self::Chunk`]: on write the writer posts
    /// [`InboundEvent::StreamSlotFreed`] so the cap replenishes the handler's
    /// outbound send window.
    WsData(Vec<u8>),
    /// A pre-serialized websocket control frame (ADR-0129) — a cap-owned pong,
    /// written verbatim. Uncredited (control frames are not paced by the
    /// handler's window), so no slot-freed event follows.
    WsControl(Vec<u8>),
    /// A pre-serialized websocket close frame (ADR-0129). Written verbatim,
    /// then the writer posts [`InboundEvent::StreamFinished`] and exits — the
    /// close handshake's final write, after which the connection tears down.
    WsClose(Vec<u8>),
}

/// Per-connection state owned by the cap dispatcher. The reader sidecar
/// holds `shutdown` + the read half; the dispatcher writes the response
/// through `write_half`.
pub struct ConnState {
    peer: SocketAddr,
    /// Dispatcher's half — used to write the HTTP/1.1 response.
    pub write_half: TcpStream,
    /// Reader thread's shutdown flag. Cap flips it + shuts down the
    /// socket to wake the blocked `read()`.
    pub shutdown: Arc<AtomicBool>,
    /// Per-connection reader control channel (ADR-0128 + keep-alive). Carries
    /// the dispatcher's streaming decision ([`ReaderControl::Buffered`] /
    /// [`ReaderControl::Stream`]), mid-stream credit replenishment
    /// ([`ReaderControl::Credit`]), and the keep-alive resume signal
    /// ([`ReaderControl::Resume`]) to the parked reader. Dropping this sender
    /// (on [`Self::close_connection`]) makes the reader's `recv_timeout` return
    /// `Disconnected`, its close-path exit.
    pub control_tx: mpsc::Sender<ReaderControl>,
    /// The `stream_id` of this connection's in-progress inbound request stream
    /// (ADR-0128), if any — the reverse index the dispatcher uses to route a
    /// reader's [`InboundEvent::RequestBodyChunk`] / `RequestBodyEnd` to the
    /// right [`RequestStreamState`]. `None` on a buffered or idle connection.
    pub active_stream: Option<u64>,
    /// Reader thread handle. Joined in `unwire`, detached on close.
    pub reader_thread: Option<JoinHandle<()>>,
    /// The connection's `Sec-WebSocket-Key`, stashed by the dispatcher when it
    /// validates an inbound RFC 6455 upgrade handshake (ADR-0129) and consumed
    /// when the handler accepts (to compute `Sec-WebSocket-Accept`). `None` on
    /// a non-upgrade connection; cleared once consumed.
    pub ws_pending_key: Option<String>,
    /// Per-connection websocket state once upgraded (ADR-0129). `None` until
    /// the handler replies `WebSocketAccept` and the cap flips the connection
    /// into websocket mode.
    pub websocket: Option<WsConn>,
}

/// Per-connection websocket state (ADR-0129), set on accept. Outbound frames
/// ride the ADR-0128 writer thread through the [`StreamState`] keyed by
/// `stream_id` in [`HttpServerCapabilityState::streams`]; `handler` is the
/// mailbox each inbound message is dispatched to.
pub struct WsConn {
    /// The handler mailbox resolved at handshake — inbound messages dispatch
    /// here, and outbound credit grants address it.
    pub handler: MailboxId,
    /// The `stream_id` of this connection's outbound writer [`StreamState`].
    pub stream_id: u64,
}

/// Bookkeeping for one in-flight request. Looked up by the dispatch's
/// auto-minted `correlation_id` (== the dispatched envelope's
/// `MailId.correlation_id`, which is also the root id since the cap
/// always dispatches via `send_envelope_as_root`).
#[derive(Copy, Clone)]
pub struct PendingRequest {
    pub conn_id: ConnId,
    /// The request's method, carried so the reply path can suppress the
    /// body on a HEAD response (message-body semantics forbid one).
    pub method: HttpMethod,
    /// Whether the reply path keeps the connection alive (renders
    /// `Connection: keep-alive` and resumes the reader) rather than
    /// closing it. Set from the [`ParsedRequest`] at dispatch.
    pub keep_alive: bool,
    /// The handler this request dispatched to. Carried so a `WebSocketAccept`
    /// reply (ADR-0129) resolves the same handler for the upgraded
    /// connection's inbound dispatch + credit grants without re-resolving.
    pub handler: MailboxId,
}

/// Per-connection response-stream state (ADR-0128), keyed in
/// [`HttpServerCapabilityState::streams`] by `stream_id` (== the request's
/// dispatch correlation id `C`). The dispatcher owns all of this
/// single-threaded, exactly like [`PendingRequest`]; the writer thread owns
/// only the socket write.
pub struct StreamState {
    /// The connection this stream writes to. Teardown paths locate a stream
    /// by connection through this field.
    conn_id: ConnId,
    /// The handler mailbox this stream's credit grants address. For a response
    /// stream (ADR-0128) it is the late-bound `handler_mailbox`; for a
    /// websocket (ADR-0129) it is the handler resolved at handshake. Stored so
    /// credit replenishment addresses the right actor without a re-lookup.
    handler: MailboxId,
    /// Bounded hand-off to the writer thread. `try_send` never blocks the
    /// dispatcher: the credit accounting keeps the invariant
    /// `credit_outstanding + queued <= window`, so a slot is always free when
    /// a within-credit chunk arrives.
    tx: mpsc::SyncSender<WriterMsg>,
    /// Writer thread handle. Detached on teardown / close (the dispatcher
    /// must never block joining a slow-peer write); joined in `unwire` after
    /// the sender is dropped.
    writer_thread: Option<JoinHandle<()>>,
    /// Credits granted to the handler it has not yet spent, bounded by the
    /// window. A chunk arriving with this at zero is an over-window flood
    /// (ADR-0128 §Consequences trust boundary) → the stream is torn down.
    credit_outstanding: u32,
    /// The handler sent `HttpResponseStreamEnd` but the terminator did not
    /// fit the bounded channel yet (chunks still queued). Flushed as slots
    /// free so the terminating chunk always follows the body in order.
    pending_end: bool,
}

/// Per-connection *inbound* request-stream state (ADR-0128), keyed in
/// [`HttpServerCapabilityState::request_streams`] by a cap-minted `stream_id`.
/// The dispatcher owns it single-threaded; the reader thread owns only the
/// socket read and the credit wait. The mirror of [`StreamState`] with the
/// data flowing the other way — the cap produces credit-paced chunks *to* the
/// handler and the handler grants credit *back*.
pub struct RequestStreamState {
    /// The connection whose reader feeds this stream. Teardown locates a
    /// stream by connection through this field.
    conn_id: ConnId,
    /// The resolved handler mailbox the cap delivers `HttpRequestChunk` /
    /// `HttpRequestStreamEnd` to. Captured at stream open so mid-stream
    /// delivery skips route re-resolution.
    handler: MailboxId,
    /// The request method, carried to the final response's [`PendingRequest`]
    /// so a HEAD response suppresses its body.
    method: HttpMethod,
    /// Whether the final response keeps the connection alive, carried to the
    /// final response's [`PendingRequest`].
    keep_alive: bool,
}

/// One registered route (ADR-0130): requests whose path matches
/// `prefix` on a segment boundary (and whose method passes `method`)
/// dispatch to `mailbox` as kind `kind`. The table keys the route by
/// the registrant's `MailboxId`, so a route survives
/// `replace_component` (the id is stable) and dispatch skips name
/// resolution.
pub struct Route {
    pub prefix: String,
    pub method: Option<HttpMethod>,
    pub kind: KindId,
    pub mailbox: MailboxId,
}

/// Segment-boundary prefix match (ADR-0130): `/api` matches `/api` and
/// `/api/…`, never `/apiary`; `/` is the catch-all. Prefixes are
/// normalized at registration ([`normalize_prefix`]), so no trailing
/// slash reaches this check.
fn route_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Validate + normalize a registration prefix: must start with `/`;
/// trailing slashes are stripped (`/api/` ⇒ `/api`) so the
/// segment-boundary match has one canonical spelling, with `/` itself
/// kept as the catch-all.
fn normalize_prefix(raw: &str) -> Result<String, String> {
    if !raw.starts_with('/') {
        return Err(format!("route prefix {raw:?} must start with '/'"));
    }
    let trimmed = raw.trim_end_matches('/');
    Ok(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

/// Registrant-mailbox validation for the explicit-`mailbox`
/// registration forms — the route twin of `aether.input`'s
/// `validate_subscriber_mailbox` (that helper lives in the input cap's
/// private runtime module, so the five-line check is mirrored rather
/// than imported). The host-stamped `_self` forms skip it: the stamp
/// already names a live in-process mailbox.
fn validate_route_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
    match registry.entry(id) {
        Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => Ok(()),
        Some(MailboxEntry::Dropped) => Err(format!("mailbox {id:?} already dropped")),
        None => Err(format!("unknown mailbox id {id:?}")),
    }
}

/// Wake sink shared with the accept + reader sidecar threads: push an
/// [`InboundEvent`] over the mpsc, then fire an [`HttpInboundReady`]
/// wake mail at the cap so the dispatcher drains.
pub struct WakeSink {
    pub inbound_tx: mpsc::Sender<InboundEvent>,
    pub mailer: Arc<Mailer>,
    pub self_id: MailboxId,
    pub wake_kind: KindId,
}

impl WakeSink {
    /// Post one event + wake. Returns `false` when the receiver is gone
    /// (the cap tore down) so the caller stops.
    pub fn post(&self, event: InboundEvent) -> bool {
        if self.inbound_tx.send(event).is_err() {
            return false;
        }
        self.mailer.push(Mail::new(
            self.self_id,
            self.wake_kind,
            HttpInboundReady::default().encode_into_bytes(),
            1,
        ));
        true
    }
}

/// `aether.http.server` runtime state. Owns one TCP listener + per-connection
/// state + the in-flight correlation table. The dispatcher holds this as the
/// cap's state and routes envelopes through the macro-emitted `Dispatch`
/// impl; the addressing identity is the distinct ZST `HttpServerCapability`.
/// Living in this private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct HttpServerCapabilityState {
    pub handler_mailbox: String,
    /// Registered routes (ADR-0130), unordered — [`Self::resolve_route`]
    /// picks the winner per request by `(prefix length, method
    /// specificity)`, which is deterministic without a sort: two
    /// distinct equal-length prefixes cannot both match one path, and
    /// duplicate `(prefix, method)` keys are rejected at registration.
    /// Route counts are tens per substrate, so the linear scan is
    /// dwarfed by the header parse that precedes it (ADR-0130).
    pub routes: Vec<Route>,
    pub max_request_bytes: usize,
    pub max_header_bytes: usize,
    /// Live-connection-table ceiling (ADR-0108 §6); a peer accepted past
    /// this is refused `503` before a reader thread is spawned.
    pub max_connections: usize,
    pub request_timeout: Duration,
    /// Idle timeout between requests on a kept-alive connection (and for a
    /// fresh connection that never sends its first byte). Distinct from
    /// `request_timeout`, which stays the in-flight read + response
    /// deadline.
    pub keep_alive_timeout: Duration,
    pub self_mailbox: MailboxId,
    /// Cached `Arc<Mailer>` so the dispatcher can fire wake mails into
    /// the cap, resolve the handler mailbox by name at dispatch time,
    /// and subscribe to settlement. The cap is single-threaded
    /// post-ADR-0038 so direct storage is fine.
    pub mailer: Arc<Mailer>,
    pub listener_port: u16,
    pub accept_shutdown: Arc<AtomicBool>,
    pub accept_thread: Option<JoinHandle<()>>,
    pub inbound_rx: mpsc::Receiver<InboundEvent>,
    pub inbound_tx: mpsc::Sender<InboundEvent>,
    pub connections: HashMap<ConnId, ConnState>,
    pub next_conn_id: ConnId,
    /// Dispatch-correlation → open response socket. Populated on
    /// dispatch; cleared on reply, settlement, timeout, or close.
    pub in_flight: HashMap<u64, PendingRequest>,
    /// Credit-window depth (ADR-0128): the count of in-flight response
    /// chunks a stream may hold; also the bounded hand-off channel's
    /// capacity and the initial credit grant.
    pub response_stream_window: u32,
    /// Active response streams (ADR-0128), keyed by `stream_id` (== the
    /// request's dispatch correlation id). Promoted from `in_flight` on
    /// `HttpResponseStreamOpen`; torn down on stream end, flood, timeout, or
    /// connection close.
    pub streams: HashMap<u64, StreamState>,
    /// Inbound request-stream credit-window depth (ADR-0128): the initial
    /// count of `HttpRequestChunk` mails the cap delivers to a streaming
    /// handler before parking the reader on the handler's `HttpRequestCredit`.
    pub request_stream_window: u32,
    /// Active *inbound* request streams (ADR-0128), keyed by a cap-minted
    /// `stream_id`. Created when a streaming handler accepts a request head;
    /// torn down when the body ends (the final response then rides the
    /// `HttpRequestStreamEnd` dispatch through `in_flight`) or on connection
    /// close.
    pub request_streams: HashMap<u64, RequestStreamState>,
    /// Monotonic source of inbound request-stream ids. Distinct from response
    /// stream ids (those reuse the request's dispatch correlation), so the two
    /// tables never collide.
    pub next_stream_id: u64,
    /// Read deadline between frames on an upgraded websocket connection
    /// (ADR-0129), from `websocket_idle_timeout_millis` — longer than
    /// `request_timeout`, since an idle websocket is normal.
    pub ws_idle_timeout: Duration,
    /// Routing index for outbound websocket frames (ADR-0129). Each inbound
    /// `WebSocketMessage` dispatches on its own fresh root; the handler answers
    /// on that same causal chain, so this maps an in-flight inbound message's
    /// root `correlation_id` to its connection. Cleared when the chain settles
    /// (`on_settled`) or the connection closes.
    pub ws_message_conn: HashMap<u64, ConnId>,
}

impl HttpServerCapabilityState {
    /// Claim `(prefix, method)` for `mailbox`, dispatching as `kind`
    /// (ADR-0130). A key held by a different mailbox is answered
    /// `Err`; the same mailbox re-claiming its own key is an
    /// idempotent `Ok` that updates `kind` — so a component
    /// re-running `wire` after `replace_component` re-registers
    /// cleanly (its `MailboxId` is stable).
    pub fn register_route(
        &mut self,
        prefix: &str,
        method: Option<HttpMethod>,
        kind: KindId,
        mailbox: MailboxId,
    ) -> RegisterRouteResult {
        let prefix = match normalize_prefix(prefix) {
            Ok(prefix) => prefix,
            Err(error) => return RegisterRouteResult::Err { error },
        };
        if let Some(existing) = self
            .routes
            .iter_mut()
            .find(|r| r.prefix == prefix && r.method == method)
        {
            if existing.mailbox == mailbox {
                existing.kind = kind;
                return RegisterRouteResult::Ok;
            }
            return RegisterRouteResult::Err {
                error: format!(
                    "route ({prefix:?}, {method:?}) already claimed by mailbox {:?}",
                    existing.mailbox,
                ),
            };
        }
        self.routes.push(Route {
            prefix,
            method,
            kind,
            mailbox,
        });
        RegisterRouteResult::Ok
    }

    /// Release the `(prefix, method)` route held by `mailbox`.
    /// Idempotent — releasing a route that isn't held (or is held by
    /// someone else) is still `Ok`, mirroring the input cap's
    /// unsubscribe semantics.
    pub fn unregister_route(
        &mut self,
        prefix: &str,
        method: Option<HttpMethod>,
        mailbox: MailboxId,
    ) -> RegisterRouteResult {
        let prefix = match normalize_prefix(prefix) {
            Ok(prefix) => prefix,
            Err(error) => return RegisterRouteResult::Err { error },
        };
        self.routes
            .retain(|r| !(r.prefix == prefix && r.method == method && r.mailbox == mailbox));
        RegisterRouteResult::Ok
    }

    /// The longest segment-boundary prefix match among
    /// method-compatible routes, with a method-specific route beating
    /// a method-agnostic one at equal prefix (ADR-0130). `None` ⇒ the
    /// configured `handler_mailbox` fallback.
    fn resolve_route(&self, path: &str, method: HttpMethod) -> Option<&Route> {
        self.routes
            .iter()
            .filter(|r| r.method.is_none_or(|m| m == method) && route_matches(&r.prefix, path))
            .max_by_key(|r| (r.prefix.len(), r.method.is_some()))
    }

    /// Allocate a fresh `ConnId`, store the connection's write half, and
    /// spin a reader thread for the read half. Refuses `503` and closes
    /// without spawning a reader when the live connection table is
    /// already at `max_connections` (ADR-0108 §6) — `connections` is the
    /// single authoritative live-connection count, so no separate
    /// counter is kept.
    pub fn spawn_reader_for_peer(&mut self, mut stream: TcpStream, peer: SocketAddr) {
        if self.connections.len() >= self.max_connections {
            let bytes = render_status_response(503, "server at connection capacity");
            let _ = stream.write_all(&bytes).and_then(|()| stream.flush());
            let _ = stream.shutdown(Shutdown::Both);
            tracing::warn!(
                target: "aether_substrate::http_server",
                %peer,
                live = self.connections.len(),
                "http conn refused: at capacity",
            );
            return;
        }

        let conn_id = self.next_conn_id;
        self.next_conn_id += 1;

        let read_half = match stream.try_clone() {
            Ok(half) => half,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::http_server",
                    %peer,
                    error = %e,
                    "http conn: try_clone failed; dropping",
                );
                return;
            }
        };
        // Slow-loris guard + response deadline (ADR-0108 §6): bound
        // every blocking read on this socket.
        if let Err(e) = read_half.set_read_timeout(Some(self.request_timeout)) {
            tracing::warn!(
                target: "aether_substrate::http_server",
                %peer,
                error = %e,
                "http conn: set_read_timeout failed; dropping",
            );
            return;
        }
        let write_half = stream;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);
        // Per-connection reader control channel (ADR-0128 + keep-alive): the
        // dispatcher's half is stored in `ConnState`, the reader's half moves
        // into the thread.
        let (control_tx, control_rx) = mpsc::channel::<ReaderControl>();

        let sink = WakeSink {
            inbound_tx: self.inbound_tx.clone(),
            mailer: Arc::clone(&self.mailer),
            self_id: self.self_mailbox,
            wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
        };
        let tuning = ReaderTuning {
            request_timeout: self.request_timeout,
            idle_timeout: self.keep_alive_timeout,
            max_header_bytes: self.max_header_bytes,
            ws_idle_timeout: self.ws_idle_timeout,
            ws_max_message_bytes: self.max_request_bytes,
        };

        // Per-connection transport reader below the mail layer — carries
        // inbound mail in; no inbound chain to inherit, no settlement
        // umbrella.
        #[allow(clippy::disallowed_methods)]
        let thread = match thread::Builder::new()
            .name(format!("aether-http-reader-{conn_id}"))
            .spawn(move || {
                run_reader_loop(
                    read_half,
                    conn_id,
                    &shutdown_for_thread,
                    &sink,
                    &control_rx,
                    tuning,
                );
            }) {
            Ok(thread) => thread,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::http_server",
                    %peer,
                    error = %e,
                    "http reader thread spawn failed",
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
                control_tx,
                active_stream: None,
                reader_thread: Some(thread),
                ws_pending_key: None,
                websocket: None,
            },
        );
        tracing::debug!(
            target: "aether_substrate::http_server",
            conn = conn_id,
            %peer,
            "http conn accepted",
        );
    }

    /// Map the method, resolve the handler, dispatch the request, and
    /// record the in-flight entry. Answers `501` / `503` inline.
    pub fn dispatch_request(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        request: ParsedRequest,
    ) {
        let Some(method) = parse_http_method(&request.method) else {
            self.write_status_response(conn_id, 501, "method not implemented");
            self.close_connection(conn_id, "unsupported method");
            return;
        };
        let keep_alive = request.keep_alive;
        // Route resolution (ADR-0130): a registered route names its
        // handler by stable `MailboxId` and the kind its requests
        // dispatch as; a dead routed mailbox is answered `503`, the
        // same surface as an unresolved fallback (falling back instead
        // would silently reroute a claimed path family). No route ⇒
        // the ADR-0108 §3 late-binding fallback: resolve the configured
        // `handler_mailbox` by name at dispatch time through the
        // registry — the sanctioned runtime-name path — dispatching
        // the generic request kind. Nothing live either way → `503`.
        let routed = self
            .resolve_route(&request.path, method)
            .map(|route| (route.mailbox, route.kind));
        let (handler, dispatch_kind) = if let Some((mailbox, kind)) = routed {
            if validate_route_mailbox(self.mailer.registry(), mailbox).is_err() {
                self.write_status_response(conn_id, 503, "routed handler gone");
                self.close_connection(conn_id, "routed mailbox dead");
                return;
            }
            (mailbox, kind)
        } else if let Some(handler) = self.mailer.registry().lookup(&self.handler_mailbox) {
            (handler, <HttpServerRequest as Kind>::ID)
        } else {
            self.write_status_response(conn_id, 503, "no handler registered");
            self.close_connection(conn_id, "handler unresolved");
            return;
        };
        let peer_addr = self
            .connections
            .get(&conn_id)
            .map(|c| c.peer.to_string())
            .unwrap_or_default();
        let payload = HttpServerRequest {
            method,
            path: request.path,
            query: request.query,
            headers: request.headers,
            body: request.body,
            peer_addr,
        }
        .encode_into_bytes();
        let mail_id = ctx.send_envelope_as_root(handler, dispatch_kind, &payload);
        // Safety net (ADR-0108 §5): if the chain settles with no
        // response, `on_settled` answers `502`. Best-effort — a chassis
        // without the settlement registry still serves the reply path.
        if let Some(registry) = self.mailer.settlement_registry() {
            registry.subscribe_settlement_mail(
                mail_id,
                self.self_mailbox,
                <Settled as Kind>::ID,
                Arc::clone(&self.mailer),
            );
        }
        self.in_flight.insert(
            mail_id.correlation_id,
            PendingRequest {
                conn_id,
                method,
                keep_alive,
                handler,
            },
        );
    }

    /// Resolve the handler a request head dispatches to — a matching ADR-0130
    /// route (its stable mailbox validated live, no fallback if it is dead) or
    /// the late-bound `handler_mailbox` — read-only, so the streaming decision
    /// can consult the resolved handler's accept-set before the body is read.
    /// `None` collapses every "nothing live" case to the `503` the buffered
    /// dispatch answers.
    fn resolved_handler(&self, path: &str, method: HttpMethod) -> Option<(MailboxId, KindId)> {
        if let Some(route) = self.resolve_route(path, method) {
            if validate_route_mailbox(self.mailer.registry(), route.mailbox).is_err() {
                return None;
            }
            return Some((route.mailbox, route.kind));
        }
        self.mailer
            .registry()
            .lookup(&self.handler_mailbox)
            .map(|handler| (handler, <HttpServerRequest as Kind>::ID))
    }

    /// Decide a parsed request head's body path (ADR-0128): resolve the
    /// handler, read its accept-set, and either signal the parked reader to
    /// buffer or stream the body, or reject and close. A handler whose
    /// accept-set carries `HttpRequestStreamOpen` takes the streamed path
    /// structurally — the cap cannot stream chunk kinds to a handler that does
    /// not handle them — so no per-request opt-in is needed.
    pub fn decide_request_head(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        head: ParsedHead,
    ) {
        let Some(method) = parse_http_method(&head.method) else {
            self.write_status_response(conn_id, 501, "method not implemented");
            self.close_connection(conn_id, "unsupported method");
            return;
        };
        let Some((handler, _kind)) = self.resolved_handler(&head.path, method) else {
            self.write_status_response(conn_id, 503, "no handler registered");
            self.close_connection(conn_id, "handler unresolved");
            return;
        };
        // ADR-0129: a websocket upgrade handshake. The cap validates the
        // protocol layer (`426`/`400`, cap-owned) before dispatch; on a valid
        // handshake it stashes the `Sec-WebSocket-Key` and dispatches the
        // request as an ordinary buffered `HttpServerRequest` the handler
        // answers with `WebSocketAccept` (accept) or `HttpServerResponse`
        // (decline). An upgrade request carries no body, so it always buffers.
        if header_has_token(&head.headers, "upgrade", "websocket") {
            match validate_ws_handshake(&head.headers) {
                Ok(key) => {
                    if let Some(conn) = self.connections.get_mut(&conn_id) {
                        conn.ws_pending_key = Some(key);
                    }
                    self.signal_reader(conn_id, ReaderControl::Buffered);
                }
                Err((status, message)) => {
                    self.write_status_response(conn_id, status, message);
                    self.close_connection(conn_id, "invalid websocket handshake");
                }
            }
            return;
        }
        let streaming = self
            .mailer
            .capability_registry()
            .accepts(handler, <HttpRequestStreamOpen as Kind>::ID);
        match (head.framing, streaming) {
            // Smuggling (`Content-Length` + `Transfer-Encoding`) and a
            // non-`chunked` coding are always `411`; a lone `chunked` body to a
            // buffered handler has no length to buffer under, also `411`
            // (relaxing #2545's guard only for a streaming handler).
            (BodyFraming::Invalid, _) | (BodyFraming::Chunked, false) => {
                self.write_status_response(conn_id, 411, "length required");
                self.close_connection(conn_id, "unbufferable request framing");
            }
            (BodyFraming::Length(n), false) if n > self.max_request_bytes => {
                self.write_status_response(conn_id, 413, "request body exceeds limit");
                self.close_connection(conn_id, "body exceeds limit");
            }
            (_, false) => {
                // Buffered handler: the reader reads the `Content-Length` body
                // into one `RequestParsed`, then `dispatch_request` runs.
                self.signal_reader(conn_id, ReaderControl::Buffered);
            }
            (_, true) => {
                self.start_request_stream(ctx, conn_id, handler, method, head);
            }
        }
    }

    /// Open an inbound request stream (ADR-0128): mint a `stream_id`, record
    /// the stream, send the handler an `HttpRequestStreamOpen`, and seed the
    /// reader's send window. The handler learns its `stream_id` here and paces
    /// the cap by mailing `HttpRequestCredit`.
    fn start_request_stream(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        handler: MailboxId,
        method: HttpMethod,
        head: ParsedHead,
    ) {
        let stream_id = self.next_stream_id;
        self.next_stream_id += 1;
        let window = self.request_stream_window.max(1);
        self.request_streams.insert(
            stream_id,
            RequestStreamState {
                conn_id,
                handler,
                method,
                keep_alive: head.keep_alive,
            },
        );
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            conn.active_stream = Some(stream_id);
        }
        let payload = HttpRequestStreamOpen {
            stream_id,
            method,
            path: head.path,
            query: head.query,
            headers: head.headers,
        }
        .encode_into_bytes();
        let _ = ctx.send_envelope_as_root(handler, <HttpRequestStreamOpen as Kind>::ID, &payload);
        self.signal_reader(conn_id, ReaderControl::Stream { credit: window });
        tracing::debug!(
            target: "aether_substrate::http_server",
            conn = conn_id,
            stream = stream_id,
            window,
            "http request stream opened",
        );
    }

    /// Forward one inbound body piece to the handler as an `HttpRequestChunk`
    /// on the connection's active stream (ADR-0128). A missing stream (the
    /// connection closed, or the stream already ended) drops the chunk.
    fn forward_request_chunk(&mut self, ctx: &mut NativeCtx<'_>, conn_id: ConnId, body: Vec<u8>) {
        let Some(stream_id) = self.connections.get(&conn_id).and_then(|c| c.active_stream) else {
            return;
        };
        let Some(handler) = self.request_streams.get(&stream_id).map(|s| s.handler) else {
            return;
        };
        let payload = HttpRequestChunk { stream_id, body }.encode_into_bytes();
        let _ = ctx.send_envelope_as_root(handler, <HttpRequestChunk as Kind>::ID, &payload);
    }

    /// Finish an inbound request stream (ADR-0128): send the handler an
    /// `HttpRequestStreamEnd` and record it as the in-flight request whose
    /// buffered `HttpServerResponse` reply, riding the terminator's envelope
    /// correlation, the reply-interception path writes back — so a streamed
    /// upload answers with one ordinary response and the settlement safety net
    /// still `502`s a handler that drops without replying.
    fn end_request_stream(&mut self, ctx: &mut NativeCtx<'_>, conn_id: ConnId) {
        let Some(stream_id) = self
            .connections
            .get_mut(&conn_id)
            .and_then(|c| c.active_stream.take())
        else {
            return;
        };
        let Some(stream) = self.request_streams.remove(&stream_id) else {
            return;
        };
        let payload = HttpRequestStreamEnd { stream_id }.encode_into_bytes();
        let mail_id =
            ctx.send_envelope_as_root(stream.handler, <HttpRequestStreamEnd as Kind>::ID, &payload);
        if let Some(registry) = self.mailer.settlement_registry() {
            registry.subscribe_settlement_mail(
                mail_id,
                self.self_mailbox,
                <Settled as Kind>::ID,
                Arc::clone(&self.mailer),
            );
        }
        self.in_flight.insert(
            mail_id.correlation_id,
            PendingRequest {
                conn_id,
                method: stream.method,
                keep_alive: stream.keep_alive,
                handler: stream.handler,
            },
        );
    }

    /// Replenish a streaming reader's send window by `credit` on the handler's
    /// grant (ADR-0128). A grant for an ended / unknown stream is a no-op.
    fn replenish_reader_credit(&mut self, stream_id: u64, credit: u32) {
        let Some(conn_id) = self.request_streams.get(&stream_id).map(|s| s.conn_id) else {
            return;
        };
        self.signal_reader(conn_id, ReaderControl::Credit { credit });
    }

    /// Send a control message to a connection's parked reader; a send failure
    /// means the reader already exited, so the connection is closed.
    fn signal_reader(&mut self, conn_id: ConnId, control: ReaderControl) {
        let sent = self
            .connections
            .get(&conn_id)
            .is_some_and(|conn| conn.control_tx.send(control).is_ok());
        if !sent {
            self.close_connection(conn_id, "reader gone");
        }
    }

    /// Format + write the handler's [`HttpServerResponse`]. `is_head`
    /// suppresses the response body per HEAD semantics (headers,
    /// including `Content-Length`, still describe what the body would
    /// have been).
    pub fn write_handler_response(
        &mut self,
        conn_id: ConnId,
        response: &HttpServerResponse,
        is_head: bool,
        keep_alive: bool,
    ) {
        let bytes = render_handler_response(response, is_head, keep_alive);
        self.write_raw_to(conn_id, &bytes);
    }

    /// Release the reader for the next request on a kept-alive connection by
    /// signalling its resume channel. A send failure means the reader
    /// already exited (its own read error / EOF), so the connection is
    /// closed instead.
    pub fn resume_connection(&mut self, conn_id: ConnId) {
        self.signal_reader(conn_id, ReaderControl::Resume);
    }

    /// Format + write a canned status response (the cap's own
    /// `413` / `431` / `501` / `502` / `503` / `504`).
    pub fn write_status_response(&mut self, conn_id: ConnId, status: u16, message: &str) {
        let bytes = render_status_response(status, message);
        self.write_raw_to(conn_id, &bytes);
    }

    fn write_raw_to(&mut self, conn_id: ConnId, bytes: &[u8]) {
        let Some(conn) = self.connections.get_mut(&conn_id) else {
            return;
        };
        if let Err(e) = conn
            .write_half
            .write_all(bytes)
            .and_then(|()| conn.write_half.flush())
        {
            tracing::debug!(
                target: "aether_substrate::http_server",
                conn = conn_id,
                error = %e,
                "http response write failed",
            );
        }
    }

    pub fn close_connection(&mut self, conn_id: ConnId, reason: &str) {
        let Some(mut conn) = self.connections.remove(&conn_id) else {
            return;
        };
        conn.shutdown.store(true, Ordering::Release);
        let _ = conn.write_half.shutdown(Shutdown::Both);
        // Detach the reader without joining inline — the dispatcher must
        // not block on it. The thread sees the shutdown (or its own EOF)
        // and exits; the JoinHandle drop detaches.
        drop(conn.reader_thread.take());
        // Tear down any response stream bound to this connection (ADR-0128).
        // The socket shutdown above unblocks a write-blocked writer; dropping
        // the sender (in `teardown_stream`) unblocks a recv-blocked one.
        let stream_ids: Vec<u64> = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.conn_id == conn_id)
            .map(|(id, _)| *id)
            .collect();
        for stream_id in stream_ids {
            self.teardown_stream(stream_id);
        }
        // Drop any inbound request stream bound to this connection (ADR-0128);
        // the reader (parked on the control channel or blocked mid-read) is
        // already unblocked by the dropped `ConnState` sender / socket
        // shutdown above.
        self.request_streams
            .retain(|_, stream| stream.conn_id != conn_id);
        // Drop any in-flight entry pinned to this connection so we don't
        // write to a dead socket.
        self.in_flight
            .retain(|_, pending| pending.conn_id != conn_id);
        // Drop websocket outbound-routing entries for this connection (ADR-0129)
        // so a late handler reply on a settled chain doesn't route to a dead
        // socket.
        self.ws_message_conn.retain(|_, mapped| *mapped != conn_id);
        tracing::debug!(
            target: "aether_substrate::http_server",
            conn = conn_id,
            peer = %conn.peer,
            reason,
            "http conn closed",
        );
    }

    /// Promote a buffered request to a response stream (ADR-0128): drop its
    /// in-flight entry so the settlement safety net no longer trips `502` on
    /// this chain, write the chunked response head, spawn the per-connection
    /// writer thread, and grant the handler its initial credit window.
    /// `stream_id` == the request's dispatch correlation id `C`.
    pub fn open_stream(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        stream_id: u64,
        conn_id: ConnId,
        open: &HttpResponseStreamOpen,
    ) {
        // The stream chain settles here; the request is no longer in-flight
        // (ADR-0128 §4), so `on_settled` won't `502` it. Resolve the handler
        // once here (it just replied, so it is live) and store it on the
        // stream for credit grants; a missing handler leaves the sentinel and
        // credit sends drop harmlessly, the pre-store no-op behavior.
        self.in_flight.remove(&stream_id);
        let handler = self
            .mailer
            .registry()
            .lookup(&self.handler_mailbox)
            .unwrap_or(MailboxId(0));
        let head = render_stream_head(open);
        self.write_raw_to(conn_id, &head);

        let Some(conn) = self.connections.get(&conn_id) else {
            // Connection already gone — nothing to stream to.
            return;
        };
        let write_half = match conn.write_half.try_clone() {
            Ok(half) => half,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::http_server",
                    conn = conn_id,
                    error = %e,
                    "http stream: writer clone failed; closing",
                );
                self.close_connection(conn_id, "stream writer clone failed");
                return;
            }
        };

        let window = self.response_stream_window.max(1);
        let (tx, rx) = mpsc::sync_channel::<WriterMsg>(window as usize);
        let sink = WakeSink {
            inbound_tx: self.inbound_tx.clone(),
            mailer: Arc::clone(&self.mailer),
            self_id: self.self_mailbox,
            wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
        };
        let idle_deadline = self.request_timeout;

        // Per-connection writer below the mail layer, mirroring the reader
        // sidecar — it owns only the socket write, never the cap state.
        #[allow(clippy::disallowed_methods)]
        let writer_thread = match thread::Builder::new()
            .name(format!("aether-http-writer-{conn_id}"))
            .spawn(move || {
                run_writer_loop(write_half, stream_id, &rx, &sink, idle_deadline);
            }) {
            Ok(thread) => thread,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::http_server",
                    conn = conn_id,
                    error = %e,
                    "http stream: writer thread spawn failed; closing",
                );
                self.close_connection(conn_id, "stream writer spawn failed");
                return;
            }
        };

        self.streams.insert(
            stream_id,
            StreamState {
                conn_id,
                handler,
                tx,
                writer_thread: Some(writer_thread),
                credit_outstanding: window,
                pending_end: false,
            },
        );
        // Grant the initial credit window — the handler learns its
        // `stream_id` from this first credit mail.
        self.send_stream_credit(ctx, stream_id, window);
        tracing::debug!(
            target: "aether_substrate::http_server",
            conn = conn_id,
            stream = stream_id,
            window,
            "http response stream opened",
        );
    }

    /// Accept one body chunk from the handler (ADR-0128): a chunk arriving
    /// with zero outstanding credit is an over-window flood and tears the
    /// stream down; otherwise it spends one credit and hands the bytes to the
    /// writer thread over the bounded channel.
    pub fn push_chunk(&mut self, stream_id: u64, body: Vec<u8>) {
        let Some((conn_id, has_credit)) = self
            .streams
            .get(&stream_id)
            .map(|stream| (stream.conn_id, stream.credit_outstanding > 0))
        else {
            // No such stream — already ended / torn down, or never opened.
            return;
        };
        if !has_credit {
            // Over-window flood (ADR-0128 §Consequences trust boundary).
            self.teardown_stream(stream_id);
            self.close_connection(conn_id, "stream credit exceeded");
            return;
        }
        let send_result = {
            let stream = self
                .streams
                .get_mut(&stream_id)
                .expect("stream present under the same borrow");
            stream.credit_outstanding -= 1;
            stream.tx.try_send(WriterMsg::Chunk(body))
        };
        if send_result.is_err() {
            // Writer gone, or (defensively) the channel was full despite the
            // credit invariant — tear the stream down rather than block.
            self.teardown_stream(stream_id);
            self.close_connection(conn_id, "stream writer unavailable");
        }
    }

    /// The handler terminated the stream (ADR-0128): hand the terminator to
    /// the writer. If the bounded channel is full it is deferred and flushed
    /// as slots free ([`Self::try_flush_end`]), so the terminating chunk
    /// always follows the body in order.
    pub fn end_stream(&mut self, stream_id: u64) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.pending_end = true;
        }
        self.try_flush_end(stream_id);
    }

    /// A writer slot freed (ADR-0128): grant one more credit to the handler
    /// (unless the stream is ending, when no more chunks are expected) and
    /// try to flush a deferred terminator.
    pub fn replenish_credit(&mut self, ctx: &mut NativeCtx<'_>, stream_id: u64) {
        let grant = match self.streams.get_mut(&stream_id) {
            Some(stream) if !stream.pending_end => {
                stream.credit_outstanding += 1;
                true
            }
            Some(_) => false,
            None => return,
        };
        if grant {
            self.send_stream_credit(ctx, stream_id, 1);
        }
        self.try_flush_end(stream_id);
    }

    /// A writer thread finished (ADR-0128) — tear the stream down and close
    /// its connection.
    pub fn finish_stream(&mut self, stream_id: u64) {
        let Some(conn_id) = self.streams.get(&stream_id).map(|stream| stream.conn_id) else {
            return;
        };
        self.teardown_stream(stream_id);
        self.close_connection(conn_id, "stream finished");
    }

    /// Try to hand a deferred terminator to the writer; on success clear the
    /// pending flag so it is sent exactly once.
    fn try_flush_end(&mut self, stream_id: u64) {
        if let Some(stream) = self.streams.get_mut(&stream_id)
            && stream.pending_end
            && stream.tx.try_send(WriterMsg::End).is_ok()
        {
            stream.pending_end = false;
        }
    }

    /// Send the stream's handler a credit grant on `stream_id`. A fresh causal
    /// root per grant keeps credit mails settling per-chunk, never holding one
    /// chain open across the stream (ADR-0128 §4). The handler is the one
    /// resolved and stored at stream open (a response stream's late-bound
    /// `handler_mailbox`, or a websocket's handshake handler, ADR-0129).
    fn send_stream_credit(&self, ctx: &mut NativeCtx<'_>, stream_id: u64, credit: u32) {
        let Some(handler) = self.streams.get(&stream_id).map(|stream| stream.handler) else {
            return;
        };
        let payload = HttpStreamCredit { stream_id, credit }.encode_into_bytes();
        let _ = ctx.send_envelope_as_root(handler, <HttpStreamCredit as Kind>::ID, &payload);
    }

    /// Remove a stream and detach its writer thread without joining inline —
    /// the dispatcher must never block on a slow-peer write. Dropping the
    /// sender unblocks a recv-waiting writer; a socket shutdown by the caller
    /// unblocks a write-blocked one.
    fn teardown_stream(&mut self, stream_id: u64) {
        if let Some(mut stream) = self.streams.remove(&stream_id) {
            drop(stream.writer_thread.take());
        }
    }

    /// Accept a websocket upgrade (ADR-0129 §1): the handler replied
    /// `WebSocketAccept` to a stashed-key upgrade request. Compute
    /// `Sec-WebSocket-Accept`, write the `101 Switching Protocols` head
    /// (inline, strictly before the writer thread starts, so the two never
    /// interleave on the shared fd), spawn the ADR-0128 writer thread for
    /// outbound frames, grant the handler its initial outbound credit window,
    /// and flip the reader into the RFC 6455 frame loop. `correlation` is the
    /// handshake request's in-flight key.
    fn accept_websocket(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        correlation: u64,
        conn_id: ConnId,
        handler: MailboxId,
        accept: &WebSocketAccept,
    ) {
        self.in_flight.remove(&correlation);
        let Some(key) = self
            .connections
            .get_mut(&conn_id)
            .and_then(|conn| conn.ws_pending_key.take())
        else {
            // A `WebSocketAccept` with no stashed key — the handler replied it
            // to a non-upgrade request. Cap-level error, close the connection.
            self.write_status_response(conn_id, 500, "websocket accept without upgrade");
            self.close_connection(conn_id, "websocket accept without upgrade");
            return;
        };
        let head = render_ws_accept(&key, accept);
        self.write_raw_to(conn_id, &head);

        let Some(conn) = self.connections.get(&conn_id) else {
            return;
        };
        let write_half = match conn.write_half.try_clone() {
            Ok(half) => half,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::http_server",
                    conn = conn_id,
                    error = %e,
                    "http websocket: writer clone failed; closing",
                );
                self.close_connection(conn_id, "websocket writer clone failed");
                return;
            }
        };

        let stream_id = self.next_stream_id;
        self.next_stream_id += 1;
        let window = self.response_stream_window.max(1);
        let (tx, rx) = mpsc::sync_channel::<WriterMsg>(window as usize);
        let sink = WakeSink {
            inbound_tx: self.inbound_tx.clone(),
            mailer: Arc::clone(&self.mailer),
            self_id: self.self_mailbox,
            wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
        };
        // The writer's idle deadline is the websocket idle timeout: an upgraded
        // socket with nothing to write for minutes is normal, unlike a stalled
        // response stream.
        let idle_deadline = self.ws_idle_timeout;

        #[allow(clippy::disallowed_methods)]
        let writer_thread = match thread::Builder::new()
            .name(format!("aether-http-writer-{conn_id}"))
            .spawn(move || {
                run_writer_loop(write_half, stream_id, &rx, &sink, idle_deadline);
            }) {
            Ok(thread) => thread,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::http_server",
                    conn = conn_id,
                    error = %e,
                    "http websocket: writer thread spawn failed; closing",
                );
                self.close_connection(conn_id, "websocket writer spawn failed");
                return;
            }
        };

        self.streams.insert(
            stream_id,
            StreamState {
                conn_id,
                handler,
                tx,
                writer_thread: Some(writer_thread),
                credit_outstanding: window,
                pending_end: false,
            },
        );
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            conn.websocket = Some(WsConn { handler, stream_id });
        }
        // Grant the initial outbound window; the handler learns its `stream_id`
        // from this first credit mail (ADR-0128 / ADR-0129 §3).
        self.send_stream_credit(ctx, stream_id, window);
        // Flip the parked reader into the frame loop.
        self.signal_reader(conn_id, ReaderControl::Upgrade);
        tracing::debug!(
            target: "aether_substrate::http_server",
            conn = conn_id,
            stream = stream_id,
            window,
            "http websocket upgraded",
        );
    }

    /// Deliver one reassembled inbound websocket message to the handler
    /// (ADR-0129 §4) as a `WebSocketMessage` on its own fresh causal root, and
    /// record the root → connection routing so the handler's answer (an
    /// outbound `WebSocketMessage` / `WebSocketClose` inheriting the chain)
    /// routes back to this socket. Subscribes to settlement so the routing
    /// entry is reclaimed when the message's chain drains.
    fn dispatch_ws_message(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        binary: bool,
        data: Vec<u8>,
    ) {
        let Some(handler) = self
            .connections
            .get(&conn_id)
            .and_then(|conn| conn.websocket.as_ref())
            .map(|ws| ws.handler)
        else {
            return;
        };
        let payload = WebSocketMessage { binary, data }.encode_into_bytes();
        let mail_id = ctx.send_envelope_as_root(handler, <WebSocketMessage as Kind>::ID, &payload);
        self.ws_message_conn.insert(mail_id.correlation_id, conn_id);
        if let Some(registry) = self.mailer.settlement_registry() {
            registry.subscribe_settlement_mail(
                mail_id,
                self.self_mailbox,
                <Settled as Kind>::ID,
                Arc::clone(&self.mailer),
            );
        }
    }

    /// Report a peer-initiated websocket close to the handler (ADR-0129 §5) as
    /// a `WebSocketClose` on its own fresh root — the inbound-close analog of
    /// [`Self::dispatch_ws_message`], so the handler observes the disconnect.
    fn report_ws_close(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        code: u16,
        reason: &str,
    ) {
        let Some(handler) = self
            .connections
            .get(&conn_id)
            .and_then(|conn| conn.websocket.as_ref())
            .map(|ws| ws.handler)
        else {
            return;
        };
        let payload = WebSocketClose {
            code,
            reason: reason.to_string(),
        }
        .encode_into_bytes();
        let _ = ctx.send_envelope_as_root(handler, <WebSocketClose as Kind>::ID, &payload);
    }

    /// Frame an outbound application message and hand it to the connection's
    /// writer thread (ADR-0129 §3). Mirrors [`Self::push_chunk`]: a message
    /// arriving with zero outstanding credit is an over-window flood and tears
    /// the connection down; otherwise it spends one credit and queues the
    /// serialized frame.
    fn send_ws_message(&mut self, conn_id: ConnId, binary: bool, data: &[u8]) {
        let Some(stream_id) = self
            .connections
            .get(&conn_id)
            .and_then(|conn| conn.websocket.as_ref())
            .map(|ws| ws.stream_id)
        else {
            return;
        };
        let opcode = if binary { OPCODE_BINARY } else { OPCODE_TEXT };
        let frame = serialize_ws_frame(opcode, data, None);
        let Some(has_credit) = self
            .streams
            .get(&stream_id)
            .map(|stream| stream.credit_outstanding > 0)
        else {
            return;
        };
        if !has_credit {
            self.teardown_stream(stream_id);
            self.close_connection(conn_id, "websocket send credit exceeded");
            return;
        }
        let send_result = {
            let stream = self
                .streams
                .get_mut(&stream_id)
                .expect("stream present under the same borrow");
            stream.credit_outstanding -= 1;
            stream.tx.try_send(WriterMsg::WsData(frame))
        };
        if send_result.is_err() {
            self.teardown_stream(stream_id);
            self.close_connection(conn_id, "websocket writer unavailable");
        }
    }

    /// Queue a cap-owned pong answering an inbound ping (ADR-0129 §5) on the
    /// connection's writer thread. Uncredited and best-effort — a full writer
    /// channel drops the pong rather than blocking or tearing down.
    fn send_ws_pong(&mut self, conn_id: ConnId, payload: &[u8]) {
        let Some(stream_id) = self
            .connections
            .get(&conn_id)
            .and_then(|conn| conn.websocket.as_ref())
            .map(|ws| ws.stream_id)
        else {
            return;
        };
        let frame = serialize_ws_frame(OPCODE_PONG, payload, None);
        if let Some(stream) = self.streams.get(&stream_id) {
            let _ = stream.tx.try_send(WriterMsg::WsControl(frame));
        }
    }

    /// Initiate / complete a websocket close (ADR-0129 §5): queue a close frame
    /// on the writer as its final write (the writer posts `StreamFinished`,
    /// which tears the connection down). Serves both a handler-initiated close
    /// and the echo of a peer close frame.
    fn send_ws_close(&mut self, conn_id: ConnId, code: u16, reason: &str) {
        let Some(stream_id) = self
            .connections
            .get(&conn_id)
            .and_then(|conn| conn.websocket.as_ref())
            .map(|ws| ws.stream_id)
        else {
            return;
        };
        let frame = serialize_ws_close_frame(code, reason);
        if let Some(stream) = self.streams.get(&stream_id) {
            if stream.tx.try_send(WriterMsg::WsClose(frame)).is_err() {
                // Writer gone — tear down directly.
                self.teardown_stream(stream_id);
                self.close_connection(conn_id, "websocket close write failed");
            }
        } else {
            self.close_connection(conn_id, "websocket already closed");
        }
    }
}

/// Outcome of [`read_more`].
enum ReadStep {
    Filled(usize),
    Eof,
    Timeout,
    Error(String),
}

/// One bounded read off the socket, retrying past `Interrupted` and
/// folding `WouldBlock` / `TimedOut` (the `set_read_timeout` expiry)
/// into [`ReadStep::Timeout`].
fn read_more(stream: &mut TcpStream, chunk: &mut [u8]) -> ReadStep {
    loop {
        match stream.read(chunk) {
            Ok(0) => return ReadStep::Eof,
            Ok(n) => return ReadStep::Filled(n),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return ReadStep::Timeout;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return ReadStep::Error(format!("read error: {e}")),
        }
    }
}

/// One parsed request head.
struct RequestHead {
    head_len: usize,
    method: String,
    path: String,
    query: String,
    headers: Vec<HttpHeader>,
    content_length: usize,
    /// How the body is framed (ADR-0128). The dispatcher turns this into the
    /// buffered / streamed / `411` decision; the reader turns it into the body
    /// read (count down `content_length`, or decode chunked).
    framing: BodyFraming,
    /// httparse minor version: `Some(0)` = HTTP/1.0, `Some(1)` = HTTP/1.1.
    /// Drives the keep-alive default ([`request_keeps_alive`]).
    version: Option<u8>,
}

/// Outcome of [`parse_head`].
enum HeadParse {
    Complete(RequestHead),
    NeedMore,
    Reject { status: u16, message: &'static str },
}

/// Percent-decode an RFC 3986 path component (ADR-0108 §2: "the decoded
/// path component"). A `%XX` escape with two following hex digits decodes
/// to the byte; a short or invalid escape (a trailing `%`, `%2`, or
/// non-hex digits like `%zz`) passes through literally rather than
/// erroring. The query string is not run through this — `+`-as-space is
/// form-encoding semantics that belongs to the query, not the path.
fn percent_decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                // `hi` / `lo` are each a single hex digit (0..=15), so
                // `hi * 16 + lo` is always in 0..=255.
                out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse the accumulated bytes as an HTTP/1.1 request head (request line
/// and headers). Enforces the header-count cap (`431` via
/// `TooManyHeaders`) and surfaces the header-byte cap to the caller via
/// [`HeadParse::NeedMore`] (the caller rejects `431` once `buf` outgrows
/// `max_header_bytes`).
fn parse_head(buf: &[u8], max_header_bytes: usize) -> HeadParse {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADER_COUNT];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(buf) {
        Ok(httparse::Status::Complete(head_len)) => {
            let method = request.method.unwrap_or_default().to_string();
            let version = request.version;
            let raw_path = request.path.unwrap_or("/");
            let (path, query) = match raw_path.split_once('?') {
                Some((before, after)) => (percent_decode_path(before), after.to_string()),
                None => (percent_decode_path(raw_path), String::new()),
            };
            let mut out_headers = Vec::with_capacity(request.headers.len());
            let mut content_length = 0usize;
            let mut has_content_length = false;
            let mut bad_length = false;
            let mut has_transfer_encoding = false;
            let mut transfer_encoding_chunked = false;
            for header in &*request.headers {
                let value = String::from_utf8_lossy(header.value).into_owned();
                if header.name.eq_ignore_ascii_case("content-length") {
                    has_content_length = true;
                    match value.trim().parse::<usize>() {
                        Ok(n) => content_length = n,
                        Err(_) => bad_length = true,
                    }
                }
                if header.name.eq_ignore_ascii_case("transfer-encoding") {
                    has_transfer_encoding = true;
                    // A lone `chunked` coding is streamable; any list (e.g.
                    // `gzip, chunked`) or non-`chunked` coding is refused.
                    if value.trim().eq_ignore_ascii_case("chunked") {
                        transfer_encoding_chunked = true;
                    }
                }
                out_headers.push(HttpHeader {
                    name: header.name.to_string(),
                    value,
                });
            }
            if bad_length {
                return HeadParse::Reject {
                    status: 400,
                    message: "invalid content-length",
                };
            }
            // Body framing (ADR-0128): `Content-Length` + `Transfer-Encoding`
            // together is request smuggling and a non-`chunked` coding is
            // unsupported — both `Invalid` (the dispatcher answers `411`); a
            // lone `chunked` streams; otherwise the `Content-Length` count (0
            // when absent) delimits the body.
            let framing = if has_transfer_encoding {
                if has_content_length || !transfer_encoding_chunked {
                    BodyFraming::Invalid
                } else {
                    BodyFraming::Chunked
                }
            } else {
                BodyFraming::Length(content_length)
            };
            HeadParse::Complete(RequestHead {
                head_len,
                method,
                path,
                query,
                headers: out_headers,
                content_length,
                framing,
                version,
            })
        }
        Ok(httparse::Status::Partial) => {
            if buf.len() > max_header_bytes {
                HeadParse::Reject {
                    status: 431,
                    message: "request header fields too large",
                }
            } else {
                HeadParse::NeedMore
            }
        }
        Err(httparse::Error::TooManyHeaders) => HeadParse::Reject {
            status: 431,
            message: "too many request headers",
        },
        Err(_) => HeadParse::Reject {
            status: 400,
            message: "malformed request",
        },
    }
}

/// Per-connection reader tuning, grouped so the reader thread body takes one
/// bundle rather than four scalars: the in-flight read + response deadline,
/// the idle timeout between requests, and the request byte caps.
#[derive(Copy, Clone)]
struct ReaderTuning {
    request_timeout: Duration,
    idle_timeout: Duration,
    max_header_bytes: usize,
    /// Read deadline between frames once the connection upgrades to a websocket
    /// (ADR-0129). The frame loop reads under this rather than `request_timeout`.
    ws_idle_timeout: Duration,
    /// Cap on a single reassembled websocket message (ADR-0129), reusing the
    /// request-body cap — an inbound message past this is a protocol error the
    /// reader answers with a close rather than buffering unboundedly.
    ws_max_message_bytes: usize,
}

/// Whether the `Connection` header names `token` (case-insensitive), across
/// comma-separated values and repeated header lines (`Connection: keep-alive,
/// Upgrade`).
fn connection_has_token(headers: &[HttpHeader], token: &str) -> bool {
    headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("connection"))
        .any(|header| {
            header
                .value
                .split(',')
                .any(|value| value.trim().eq_ignore_ascii_case(token))
        })
}

/// First value of the header named `name` (case-insensitive), or `None` if the
/// request carries no such header. Trimmed of surrounding whitespace.
fn first_header_value<'a>(headers: &'a [HttpHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.trim())
}

/// Whether the header named `name` lists `token` (case-insensitive), across
/// comma-separated values and repeated header lines — the general form of
/// [`connection_has_token`], used for `Upgrade: websocket`.
fn header_has_token(headers: &[HttpHeader], name: &str, token: &str) -> bool {
    headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name))
        .any(|header| {
            header
                .value
                .split(',')
                .any(|value| value.trim().eq_ignore_ascii_case(token))
        })
}

/// Validate an RFC 6455 upgrade handshake's protocol layer (ADR-0129 §1),
/// cap-owned before any dispatch. The caller has already seen `Upgrade:
/// websocket`; this checks `Connection: Upgrade`, `Sec-WebSocket-Version: 13`,
/// and a non-empty `Sec-WebSocket-Key`, returning the key on success or the
/// canned status the cap answers (`426` for a wrong version, `400` for a
/// malformed handshake) on failure.
fn validate_ws_handshake(headers: &[HttpHeader]) -> Result<String, (u16, &'static str)> {
    if !connection_has_token(headers, "upgrade") {
        return Err((400, "websocket upgrade requires Connection: Upgrade"));
    }
    if first_header_value(headers, "sec-websocket-version") != Some("13") {
        return Err((426, "unsupported websocket version"));
    }
    match first_header_value(headers, "sec-websocket-key") {
        Some(key) if !key.is_empty() => Ok(key.to_string()),
        _ => Err((400, "missing sec-websocket-key")),
    }
}

/// Whether a request wants its connection kept alive after the response.
/// An explicit `Connection` token wins either way; absent one, the HTTP
/// version decides — HTTP/1.1 (`Some(1)`) keeps alive by default, HTTP/1.0
/// (`Some(0)`, or an unknown/absent version) closes by default.
fn request_keeps_alive(version: Option<u8>, headers: &[HttpHeader]) -> bool {
    if connection_has_token(headers, "close") {
        return false;
    }
    if connection_has_token(headers, "keep-alive") {
        return true;
    }
    matches!(version, Some(1))
}

/// Cap on a chunked-transfer size line (hex size + optional `;ext`) and on a
/// trailer header line, in bytes — a peer cannot stream an unbounded framing
/// line at the cap.
const MAX_CHUNK_LINE_BYTES: usize = 256;
/// Cap on trailer header lines after the terminating chunk — a peer cannot
/// stream unbounded trailers.
const MAX_CHUNK_TRAILERS: usize = MAX_HEADER_COUNT;

/// Per-connection reader thread body. An outer per-request loop reads one
/// HTTP/1.1 request head, posts it for the dispatcher's streaming decision,
/// then reads the body either buffered (`Content-Length` into one
/// [`InboundEvent::RequestParsed`]) or streamed (credit-paced
/// [`InboundEvent::RequestBodyChunk`] mails, ADR-0128), then waits on the
/// per-connection control channel: on a keep-alive response the dispatcher
/// signals [`ReaderControl::Resume`] and the loop reads the next request off
/// the same socket (carrying any over-read pipelined bytes forward); on a
/// close response the dispatcher drops the sender and the reader exits. A fresh
/// / idle connection between requests reads under `idle_timeout`; once a
/// request's bytes start arriving the in-flight `request_timeout` (slow-loris)
/// governs, and the handler-response deadline is the control wait.
#[allow(clippy::too_many_lines)]
fn run_reader_loop(
    read_half: TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    sink: &WakeSink,
    control_rx: &mpsc::Receiver<ReaderControl>,
    tuning: ReaderTuning,
) {
    let ReaderTuning {
        request_timeout,
        idle_timeout,
        max_header_bytes,
        ..
    } = tuning;
    let mut stream = read_half;
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    // `spawn_reader_for_peer` set the socket read timeout to `request_timeout`
    // before spawning; track it so a read only re-issues `set_read_timeout`
    // when the desired timeout actually changes.
    let mut current_timeout = request_timeout;

    // Outer per-request loop (keep-alive): serve requests in sequence on one
    // socket, one in flight at a time.
    loop {
        // Phase 1: accumulate the request head. Before the first byte of a
        // new request the read waits under the idle timeout (a kept-alive /
        // fresh connection between requests); once head bytes are buffered
        // it waits under the in-flight request timeout (slow-loris).
        let head = loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match parse_head(&buf, max_header_bytes) {
                HeadParse::Complete(head) => break head,
                HeadParse::Reject { status, message } => {
                    sink.post(InboundEvent::RequestRejected {
                        conn_id,
                        status,
                        message,
                    });
                    return;
                }
                HeadParse::NeedMore => {}
            }
            let want_timeout = if buf.is_empty() {
                idle_timeout
            } else {
                request_timeout
            };
            if want_timeout != current_timeout {
                if stream.set_read_timeout(Some(want_timeout)).is_err() {
                    sink.post(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: "set read timeout failed".to_string(),
                    });
                    return;
                }
                current_timeout = want_timeout;
            }
            match read_more(&mut stream, &mut chunk) {
                ReadStep::Filled(n) => buf.extend_from_slice(&chunk[..n]),
                ReadStep::Eof => {
                    let reason = if buf.is_empty() {
                        "eof between requests"
                    } else {
                        "eof before request head"
                    };
                    sink.post(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: reason.to_string(),
                    });
                    return;
                }
                ReadStep::Timeout => {
                    // An idle-timeout expiry with no partial request buffered
                    // is a silent close of a kept-alive / fresh idle
                    // connection; a timeout mid-head is the slow-loris guard.
                    let reason = if buf.is_empty() {
                        "idle"
                    } else {
                        "read timeout (head)"
                    };
                    sink.post(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: reason.to_string(),
                    });
                    return;
                }
                ReadStep::Error(reason) => {
                    sink.post(InboundEvent::ReaderClosed { conn_id, reason });
                    return;
                }
            }
        };

        // The body read and the `Expect: 100-continue` write run under the
        // in-flight timeout — a request has started arriving.
        if current_timeout != request_timeout {
            if stream.set_read_timeout(Some(request_timeout)).is_err() {
                sink.post(InboundEvent::ReaderClosed {
                    conn_id,
                    reason: "set read timeout failed".to_string(),
                });
                return;
            }
            current_timeout = request_timeout;
        }

        let keep_alive = request_keeps_alive(head.version, &head.headers);

        // Post the head and await the dispatcher's streaming decision (ADR-0128).
        // The dispatcher owns registry access, so it resolves the handler,
        // reads its accept-set, and replies `Buffered` / `Stream` — or rejects
        // (`411` / `413` / `501` / `503`) and closes, surfacing here as a
        // disconnect. Reject decisions (including the body-cap `413` that used
        // to live in this reader) now belong to the dispatcher, which has the
        // handler's streaming disposition.
        let parsed_head = ParsedHead {
            method: head.method.clone(),
            path: head.path.clone(),
            query: head.query.clone(),
            headers: head.headers.clone(),
            framing: head.framing,
            keep_alive,
        };
        if !sink.post(InboundEvent::RequestHeadParsed {
            conn_id,
            head: parsed_head,
        }) {
            return;
        }
        // A timeout means the dispatcher never answered (a wedged / torn-down
        // cap); a disconnect means it rejected the request and closed. Either
        // way there is nothing more this reader can write.
        let Ok(mode) = control_rx.recv_timeout(request_timeout) else {
            return;
        };

        // `Expect: 100-continue`: written only once the dispatcher has accepted
        // the body (a rejected request never prompts a body send). The reader
        // owns a full-duplex clone of the socket, so it writes the interim
        // response inline, strictly before the body read — the final response
        // still goes out the dispatcher's `write_half`, so the two writes never
        // interleave on the shared fd.
        let expects_continue = head.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("expect")
                && header.value.to_ascii_lowercase().contains("100-continue")
        });
        if expects_continue && let Err(e) = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n") {
            tracing::debug!(
                target: "aether_substrate::http_server",
                conn = conn_id,
                error = %e,
                "http conn: 100-continue write failed",
            );
        }

        // Phase 2: read the body per the decision, then Phase 3: wait for the
        // response deadline. Both paths leave the reader ready to loop with the
        // over-read (pipelined) bytes in `next_buf`, or return to close.
        let next_buf = match mode {
            ReaderControl::Buffered => {
                match read_buffered_body(&mut stream, conn_id, shutdown, sink, &head, &buf) {
                    Some((body, next_buf)) => {
                        let request = ParsedRequest {
                            method: head.method,
                            path: head.path,
                            query: head.query,
                            headers: head.headers,
                            body,
                            keep_alive,
                        };
                        if !sink.post(InboundEvent::RequestParsed { conn_id, request }) {
                            return;
                        }
                        next_buf
                    }
                    None => return,
                }
            }
            ReaderControl::Stream { credit } => {
                let leftover = buf[head.head_len..].to_vec();
                match stream_request_body(
                    &mut stream,
                    conn_id,
                    shutdown,
                    sink,
                    control_rx,
                    head.framing,
                    leftover,
                    credit,
                    request_timeout,
                ) {
                    StreamOutcome::Complete { next_buf } => next_buf,
                    StreamOutcome::Closed => return,
                }
            }
            // The dispatcher never sends a bare credit / resume / upgrade as
            // the first control after a head (an upgrade rides the Buffered
            // dispatch, then arrives at the response wait) — treat it as a
            // torn-down connection.
            ReaderControl::Credit { .. } | ReaderControl::Resume | ReaderControl::Upgrade => return,
        };

        // Phase 3: response deadline. Wait for the dispatcher's resume signal
        // (a keep-alive response was written — loop to the next request), the
        // handler-response timeout (`504`), or the sender being dropped (the
        // dispatcher took the close path — the connection is torn down).
        match control_rx.recv_timeout(request_timeout) {
            Ok(ReaderControl::Resume) => {
                buf = next_buf;
            }
            Ok(ReaderControl::Upgrade) => {
                // ADR-0129: the handshake was accepted (`101` written) — leave
                // the HTTP request lifecycle for the RFC 6455 frame loop,
                // carrying any bytes over-read past the handshake head.
                run_ws_reader_loop(&mut stream, conn_id, shutdown, sink, next_buf, tuning);
                return;
            }
            Ok(_) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                sink.post(InboundEvent::RequestTimedOut { conn_id });
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return;
            }
        }
    }
}

/// Read a `Content-Length`-delimited body into one buffer (the buffered path).
/// Leftover bytes already past the head come first; any over-read past the body
/// (a pipelined next request) is returned as the second tuple element so
/// keep-alive framing stays intact. `None` on EOF / timeout / error / shutdown
/// (a `ReaderClosed` is posted where appropriate).
fn read_buffered_body(
    stream: &mut TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    sink: &WakeSink,
    head: &RequestHead,
    buf: &[u8],
) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut body: Vec<u8> = Vec::with_capacity(head.content_length);
    let mut next_buf: Vec<u8> = Vec::new();
    let after_head = &buf[head.head_len..];
    if after_head.len() >= head.content_length {
        body.extend_from_slice(&after_head[..head.content_length]);
        next_buf.extend_from_slice(&after_head[head.content_length..]);
    } else {
        body.extend_from_slice(after_head);
    }
    let mut chunk = [0u8; 8 * 1024];
    while body.len() < head.content_length {
        if shutdown.load(Ordering::Acquire) {
            return None;
        }
        match read_more(stream, &mut chunk) {
            ReadStep::Filled(n) => {
                let want = head.content_length - body.len();
                if n <= want {
                    body.extend_from_slice(&chunk[..n]);
                } else {
                    body.extend_from_slice(&chunk[..want]);
                    next_buf.extend_from_slice(&chunk[want..n]);
                }
            }
            ReadStep::Eof => {
                sink.post(InboundEvent::ReaderClosed {
                    conn_id,
                    reason: "eof mid-body".to_string(),
                });
                return None;
            }
            ReadStep::Timeout => {
                sink.post(InboundEvent::ReaderClosed {
                    conn_id,
                    reason: "read timeout (body)".to_string(),
                });
                return None;
            }
            ReadStep::Error(reason) => {
                sink.post(InboundEvent::ReaderClosed { conn_id, reason });
                return None;
            }
        }
    }
    Some((body, next_buf))
}

/// Outcome of streaming one request body to the dispatcher (ADR-0128).
enum StreamOutcome {
    /// The body completed and [`InboundEvent::RequestBodyEnd`] was posted;
    /// `next_buf` holds any over-read bytes past the body (a pipelined request).
    Complete { next_buf: Vec<u8> },
    /// The reader stopped (EOF / timeout / error / teardown); a `ReaderClosed`
    /// was posted where the stop was the reader's, or the control / wake
    /// channel disconnected.
    Closed,
}

/// Stream a request body to the dispatcher as credit-paced
/// [`InboundEvent::RequestBodyChunk`] mails, then post
/// [`InboundEvent::RequestBodyEnd`] (ADR-0128). A `Content-Length` body counts
/// down; a `chunked` body is decoded frame by frame. The reader parks on the
/// control channel when its send window is exhausted, so a fast peer backs up
/// into TCP rather than growing the handler's inbox.
#[allow(clippy::too_many_arguments)]
fn stream_request_body(
    stream: &mut TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    sink: &WakeSink,
    control_rx: &mpsc::Receiver<ReaderControl>,
    framing: BodyFraming,
    leftover: Vec<u8>,
    initial_credit: u32,
    request_timeout: Duration,
) -> StreamOutcome {
    let mut streamer = BodyStreamer {
        stream,
        conn_id,
        shutdown,
        sink,
        control_rx,
        request_timeout,
        credit: initial_credit,
        work: leftover,
        pos: 0,
    };
    let result = match framing {
        BodyFraming::Length(n) => streamer.stream_length_body(n),
        BodyFraming::Chunked => streamer.stream_chunked_body(),
        // The dispatcher never signals `Stream` for `Invalid` framing.
        BodyFraming::Invalid => Err(()),
    };
    match result {
        Ok(()) => {
            let next_buf = streamer.work[streamer.pos..].to_vec();
            if sink.post(InboundEvent::RequestBodyEnd { conn_id }) {
                StreamOutcome::Complete { next_buf }
            } else {
                StreamOutcome::Closed
            }
        }
        Err(()) => StreamOutcome::Closed,
    }
}

/// The reader-side state for streaming one request body (ADR-0128): the socket,
/// a working buffer with a cursor over unconsumed bytes, and the send-window
/// credit shared with the dispatcher through the control channel. All methods
/// return `Err(())` to mean "stop, the outcome is [`StreamOutcome::Closed`]",
/// posting a `ReaderClosed` first where the stop is a read failure.
struct BodyStreamer<'a> {
    stream: &'a mut TcpStream,
    conn_id: ConnId,
    shutdown: &'a AtomicBool,
    sink: &'a WakeSink,
    control_rx: &'a mpsc::Receiver<ReaderControl>,
    request_timeout: Duration,
    /// Un-spent send-window credit — the count of `RequestBodyChunk` mails the
    /// reader may still post before it must park for the handler's grant.
    credit: u32,
    /// Unconsumed bytes (leftover past the head, plus socket refills).
    work: Vec<u8>,
    /// Cursor into `work`; bytes before it are consumed.
    pos: usize,
}

impl BodyStreamer<'_> {
    /// Unconsumed byte count.
    fn avail(&self) -> usize {
        self.work.len() - self.pos
    }

    fn post_closed(&self, reason: &str) {
        self.sink.post(InboundEvent::ReaderClosed {
            conn_id: self.conn_id,
            reason: reason.to_string(),
        });
    }

    /// Read one socket batch into `work`, compacting the consumed prefix first.
    /// `Err` on EOF / timeout / error / shutdown.
    fn read_socket_once(&mut self) -> Result<(), ()> {
        if self.pos > 0 {
            self.work.drain(..self.pos);
            self.pos = 0;
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(());
        }
        let mut chunk = [0u8; 8 * 1024];
        match read_more(self.stream, &mut chunk) {
            ReadStep::Filled(n) => {
                self.work.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            ReadStep::Eof => {
                self.post_closed("eof mid-body");
                Err(())
            }
            ReadStep::Timeout => {
                self.post_closed("read timeout (body)");
                Err(())
            }
            ReadStep::Error(reason) => {
                self.sink.post(InboundEvent::ReaderClosed {
                    conn_id: self.conn_id,
                    reason,
                });
                Err(())
            }
        }
    }

    /// Ensure at least one unconsumed byte, reading from the socket if needed.
    fn ensure_avail(&mut self) -> Result<(), ()> {
        if self.avail() == 0 {
            self.read_socket_once()?;
        }
        Ok(())
    }

    /// Wait for a send-window credit, then deliver `body` as one inbound chunk
    /// and spend the credit (ADR-0128). An empty body is never framed. A zero
    /// window parks on the control channel until the handler grants more; a
    /// stall past `request_timeout` or a teardown returns `Err`.
    fn emit(&mut self, body: Vec<u8>) -> Result<(), ()> {
        if body.is_empty() {
            return Ok(());
        }
        while self.credit == 0 {
            if self.shutdown.load(Ordering::Acquire) {
                return Err(());
            }
            match self.control_rx.recv_timeout(self.request_timeout) {
                Ok(ReaderControl::Credit { credit } | ReaderControl::Stream { credit }) => {
                    self.credit = self.credit.saturating_add(credit);
                }
                // A bare buffered / resume / upgrade mid-stream, or the control
                // channel dropped, means a torn-down connection — stop.
                Ok(ReaderControl::Buffered | ReaderControl::Resume | ReaderControl::Upgrade)
                | Err(mpsc::RecvTimeoutError::Disconnected) => return Err(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.post_closed("stream credit timeout");
                    return Err(());
                }
            }
        }
        if !self.sink.post(InboundEvent::RequestBodyChunk {
            conn_id: self.conn_id,
            body,
        }) {
            return Err(());
        }
        self.credit -= 1;
        Ok(())
    }

    /// Deliver the next `remaining`-bounded run of unconsumed bytes as one
    /// chunk, refilling from the socket when empty. Returns the new remaining.
    fn deliver_slice(&mut self, remaining: usize) -> Result<usize, ()> {
        self.ensure_avail()?;
        let take = remaining.min(self.avail());
        let slice = self.work[self.pos..self.pos + take].to_vec();
        self.pos += take;
        self.emit(slice)?;
        Ok(remaining - take)
    }

    /// Stream a `Content-Length`-delimited body of `n` bytes.
    fn stream_length_body(&mut self, n: usize) -> Result<(), ()> {
        let mut remaining = n;
        while remaining > 0 {
            remaining = self.deliver_slice(remaining)?;
        }
        Ok(())
    }

    /// Stream a `chunked` transfer-encoded body, decoding it frame by frame:
    /// a hex chunk-size line, that many body bytes (delivered credit-paced),
    /// its trailing CRLF, repeated until the zero-length terminator, then any
    /// trailer up to the final blank line.
    fn stream_chunked_body(&mut self) -> Result<(), ()> {
        loop {
            let size = self.read_chunk_size()?;
            if size == 0 {
                return self.consume_trailer();
            }
            let mut remaining = size;
            while remaining > 0 {
                remaining = self.deliver_slice(remaining)?;
            }
            self.consume_crlf()?;
        }
    }

    /// Read and parse a chunk-size line: hex digits up to an optional `;ext`,
    /// terminated by CRLF. `Err` on a malformed or oversize line.
    fn read_chunk_size(&mut self) -> Result<usize, ()> {
        let line = self.read_line()?;
        let hex_end = line.iter().position(|&b| b == b';').unwrap_or(line.len());
        let text = from_utf8(&line[..hex_end]).unwrap_or("").trim();
        usize::from_str_radix(text, 16).map_err(|_| self.post_closed("malformed chunk size"))
    }

    /// Read one CRLF-terminated line (the CRLF stripped), bounded to
    /// [`MAX_CHUNK_LINE_BYTES`]. The line stays in `work` across refills so a
    /// CRLF split across two socket reads still matches.
    fn read_line(&mut self) -> Result<Vec<u8>, ()> {
        loop {
            if let Some(idx) = self.work[self.pos..].windows(2).position(|w| w == b"\r\n") {
                let line = self.work[self.pos..self.pos + idx].to_vec();
                self.pos += idx + 2;
                return Ok(line);
            }
            if self.avail() > MAX_CHUNK_LINE_BYTES {
                self.post_closed("chunk line too long");
                return Err(());
            }
            self.read_socket_once()?;
        }
    }

    /// Consume the CRLF that follows a chunk's data bytes.
    fn consume_crlf(&mut self) -> Result<(), ()> {
        while self.avail() < 2 {
            self.read_socket_once()?;
        }
        if &self.work[self.pos..self.pos + 2] == b"\r\n" {
            self.pos += 2;
            Ok(())
        } else {
            self.post_closed("malformed chunk framing");
            Err(())
        }
    }

    /// Consume trailer header lines after the terminating chunk, up to the
    /// final blank line. Trailers are ignored (our subset carries none); the
    /// count is bounded so a peer cannot stream them unboundedly.
    fn consume_trailer(&mut self) -> Result<(), ()> {
        for _ in 0..MAX_CHUNK_TRAILERS {
            if self.read_line()?.is_empty() {
                return Ok(());
            }
        }
        self.post_closed("too many chunk trailers");
        Err(())
    }
}

/// Headers the cap supplies itself (ADR-0108 §2) — stripped from a
/// handler's response so they aren't doubled.
fn is_cap_owned_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("date")
        || name.eq_ignore_ascii_case("transfer-encoding")
}

/// Render the handler's [`HttpServerResponse`] as an HTTP/1.1 response,
/// supplying `Content-Length` / `Date` / `Connection` (ADR-0108 §2).
/// `is_head` emits the full head — including the `Content-Length` the
/// body would have had — but appends no body bytes (HEAD semantics: a
/// message with no message body). `keep_alive` renders `Connection:
/// keep-alive` (the connection is held for the next request) vs
/// `Connection: close`.
fn render_handler_response(
    response: &HttpServerResponse,
    is_head: bool,
    keep_alive: bool,
) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason_phrase(response.status)
    );
    for header in &response.headers {
        if is_cap_owned_header(&header.name) {
            continue;
        }
        let _ = write!(head, "{}: {}\r\n", header.name, header.value);
    }
    let _ = write!(head, "Content-Length: {}\r\n", response.body.len());
    let _ = write!(head, "Date: {}\r\n", http_date(SystemTime::now()));
    if keep_alive {
        head.push_str("Connection: keep-alive\r\n\r\n");
    } else {
        head.push_str("Connection: close\r\n\r\n");
    }
    let mut out = head.into_bytes();
    if !is_head {
        out.extend_from_slice(&response.body);
    }
    out
}

/// Render a canned status response with a plain-text body.
fn render_status_response(status: u16, message: &str) -> Vec<u8> {
    use std::fmt::Write as _;
    let body = message.as_bytes();
    let mut head = format!("HTTP/1.1 {} {}\r\n", status, reason_phrase(status));
    head.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    let _ = write!(head, "Content-Length: {}\r\n", body.len());
    let _ = write!(head, "Date: {}\r\n", http_date(SystemTime::now()));
    head.push_str("Connection: close\r\n\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Render the response head for a streamed response (ADR-0128): the status
/// line, the handler's headers (cap-owned ones stripped), `Transfer-Encoding:
/// chunked` in place of `Content-Length`, and `Date` / `Connection`.
fn render_stream_head(open: &HttpResponseStreamOpen) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        open.status,
        reason_phrase(open.status)
    );
    for header in &open.headers {
        if is_cap_owned_header(&header.name) {
            continue;
        }
        let _ = write!(head, "{}: {}\r\n", header.name, header.value);
    }
    head.push_str("Transfer-Encoding: chunked\r\n");
    let _ = write!(head, "Date: {}\r\n", http_date(SystemTime::now()));
    head.push_str("Connection: close\r\n\r\n");
    head.into_bytes()
}

/// Per-connection response-stream writer thread body (ADR-0128). Drains the
/// bounded hand-off channel, framing each chunk as chunked transfer-encoding
/// and writing the terminating chunk on `End`. TCP backpressure blocks this
/// thread on the socket write, never the dispatcher. A `recv_timeout` past
/// `idle_deadline` is the idle-write / no-progress deadline: a handler that
/// stalled mid-stream tears the stream down.
fn run_writer_loop(
    write_half: TcpStream,
    stream_id: u64,
    rx: &mpsc::Receiver<WriterMsg>,
    sink: &WakeSink,
    idle_deadline: Duration,
) {
    let mut write_half = write_half;
    loop {
        match rx.recv_timeout(idle_deadline) {
            Ok(WriterMsg::Chunk(body)) => {
                if write_chunk(&mut write_half, &body).is_err() {
                    // Peer disconnected / socket error mid-stream.
                    sink.post(InboundEvent::StreamFinished { stream_id });
                    return;
                }
                // One window slot freed — let the dispatcher replenish credit.
                if !sink.post(InboundEvent::StreamSlotFreed { stream_id }) {
                    return;
                }
            }
            Ok(WriterMsg::End) => {
                let _ = write_terminator(&mut write_half);
                sink.post(InboundEvent::StreamFinished { stream_id });
                return;
            }
            Ok(WriterMsg::WsData(frame)) => {
                // A pre-serialized websocket data frame (ADR-0129): write it
                // verbatim, then free a credit slot like a response chunk.
                if write_all_flush(&mut write_half, &frame).is_err() {
                    sink.post(InboundEvent::StreamFinished { stream_id });
                    return;
                }
                if !sink.post(InboundEvent::StreamSlotFreed { stream_id }) {
                    return;
                }
            }
            Ok(WriterMsg::WsControl(frame)) => {
                // A cap-owned pong (ADR-0129 §5): verbatim, uncredited — no
                // slot-freed event, so it never inflates the handler's window.
                if write_all_flush(&mut write_half, &frame).is_err() {
                    sink.post(InboundEvent::StreamFinished { stream_id });
                    return;
                }
            }
            Ok(WriterMsg::WsClose(frame)) => {
                // The close handshake's final write (ADR-0129 §5): verbatim,
                // then finish so the dispatcher tears the connection down.
                let _ = write_all_flush(&mut write_half, &frame);
                sink.post(InboundEvent::StreamFinished { stream_id });
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle-write deadline (ADR-0128 §4): the handler stalled.
                sink.post(InboundEvent::StreamFinished { stream_id });
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The dispatcher dropped the sender (stream torn down
                // elsewhere) — nothing more to write.
                return;
            }
        }
    }
}

/// Frame one body piece as a chunked transfer-encoding chunk (`hexlen CRLF
/// body CRLF`) and flush it. An empty body is skipped rather than framed — a
/// zero-length chunk is the transfer terminator, which only `End` may write.
fn write_chunk(write_half: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    let header = format!("{:x}\r\n", body.len());
    write_half.write_all(header.as_bytes())?;
    write_half.write_all(body)?;
    write_half.write_all(b"\r\n")?;
    write_half.flush()
}

/// Write the chunked transfer terminator (the zero-length chunk) and flush.
fn write_terminator(write_half: &mut TcpStream) -> io::Result<()> {
    write_half.write_all(b"0\r\n\r\n")?;
    write_half.flush()
}

/// Write `bytes` verbatim and flush — a pre-serialized websocket frame
/// (ADR-0129), with no chunked-transfer framing layered on.
fn write_all_flush(write_half: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    write_half.write_all(bytes)?;
    write_half.flush()
}

// ADR-0129 websocket support: the RFC 6455 handshake accept-key (hand-rolled
// SHA-1 + base64), the frame codec (parse / serialize / masking / continuation
// reassembly), the `101` render, and the reader-thread frame loop. All under
// `feature = "runtime"` with the rest of this module — the wasm-safe `kinds.rs`
// half is untouched.

/// RFC 6455 §1.3 handshake GUID, concatenated with the client
/// `Sec-WebSocket-Key` before the SHA-1 that forms `Sec-WebSocket-Accept`.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// RFC 6455 opcodes the cap emits / matches.
const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

/// Compute `Sec-WebSocket-Accept` (RFC 6455 §4.2.2): base64(SHA-1(key + GUID)).
fn sec_websocket_accept(key: &str) -> String {
    let mut input = key.as_bytes().to_vec();
    input.extend_from_slice(WS_GUID.as_bytes());
    BASE64_STANDARD.encode(sha1(&input))
}

/// Hand-rolled SHA-1 (RFC 3174) over `data`, returning the 20-byte digest. Not
/// a general crypto primitive — a fixed, short, non-secret hash for the RFC
/// 6455 handshake (ADR-0129 §6), pinned by the RFC's own worked vector rather
/// than earning a `sha1` crate. SHA-1 is broken for collision resistance; the
/// handshake does not rely on that property (it echoes a nonce), so this use is
/// correct.
// SHA-1's working variables are single letters by the RFC 3174 spec
// (a/b/c/d/e/f/h/w/k); renaming them would obscure the transcription.
#[allow(clippy::many_single_char_names)]
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    // Pad: append 0x80, zero-fill to 56 mod 64, then the 64-bit big-endian
    // message length in bits.
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for block in msg.chunks_exact(64) {
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let j = i * 4;
            *word = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Serialize one RFC 6455 frame with FIN set: `opcode`, `payload`, optionally
/// masked with `mask`. A server→client frame is unmasked (`mask = None`, ADR-0129
/// §3); the masked path exists for the codec's own round-trip tests.
fn serialize_ws_frame(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0F));
    let masked_bit: u8 = if mask.is_some() { 0x80 } else { 0x00 };
    let len = payload.len();
    if len < 126 {
        out.push(masked_bit | u8::try_from(len).unwrap_or(0));
    } else if let Ok(short) = u16::try_from(len) {
        out.push(masked_bit | 0x7E);
        out.extend_from_slice(&short.to_be_bytes());
    } else {
        out.push(masked_bit | 0x7F);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    match mask {
        Some(key) => {
            out.extend_from_slice(&key);
            for (i, byte) in payload.iter().enumerate() {
                out.push(byte ^ key[i % 4]);
            }
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// Serialize an RFC 6455 close frame (opcode `0x8`): a 2-byte big-endian status
/// code followed by the UTF-8 reason, unmasked.
fn serialize_ws_close_frame(code: u16, reason: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + reason.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(reason.as_bytes());
    serialize_ws_frame(OPCODE_CLOSE, &payload, None)
}

/// One parsed RFC 6455 frame — header decoded, payload unmasked.
struct WsFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Outcome of [`parse_ws_frame`].
enum WsFrameParse {
    Complete {
        frame: WsFrame,
        consumed: usize,
    },
    NeedMore,
    /// A protocol violation (an unmasked client frame, a reserved bit set, or a
    /// length past the cap). Carries the RFC 6455 close code + reason the cap
    /// answers before tearing down — untrusted input never panics (ADR-0129).
    Error {
        code: u16,
        reason: &'static str,
    },
}

/// Parse one frame from the front of `buf`. A client→server frame MUST be
/// masked (RFC 6455 §5.1); `max_payload` bounds one frame's payload so an
/// oversize length is a protocol error, not an allocation.
fn parse_ws_frame(buf: &[u8], max_payload: usize) -> WsFrameParse {
    if buf.len() < 2 {
        return WsFrameParse::NeedMore;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    if b0 & 0x70 != 0 {
        return WsFrameParse::Error {
            code: 1002,
            reason: "reserved bits set",
        };
    }
    let fin = b0 & 0x80 != 0;
    let opcode = b0 & 0x0F;
    if b1 & 0x80 == 0 {
        return WsFrameParse::Error {
            code: 1002,
            reason: "client frame not masked",
        };
    }
    let len7 = usize::from(b1 & 0x7F);
    let mut cursor = 2;
    let payload_len = match len7 {
        126 => {
            if buf.len() < cursor + 2 {
                return WsFrameParse::NeedMore;
            }
            let n = usize::from(u16::from_be_bytes([buf[cursor], buf[cursor + 1]]));
            cursor += 2;
            n
        }
        127 => {
            if buf.len() < cursor + 8 {
                return WsFrameParse::NeedMore;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&buf[cursor..cursor + 8]);
            cursor += 8;
            match usize::try_from(u64::from_be_bytes(arr)) {
                Ok(n) => n,
                Err(_) => {
                    return WsFrameParse::Error {
                        code: 1009,
                        reason: "frame too large",
                    };
                }
            }
        }
        n => n,
    };
    if payload_len > max_payload {
        return WsFrameParse::Error {
            code: 1009,
            reason: "frame too large",
        };
    }
    if buf.len() < cursor + 4 + payload_len {
        return WsFrameParse::NeedMore;
    }
    let mask = [
        buf[cursor],
        buf[cursor + 1],
        buf[cursor + 2],
        buf[cursor + 3],
    ];
    cursor += 4;
    let mut payload = Vec::with_capacity(payload_len);
    for (i, &byte) in buf[cursor..cursor + payload_len].iter().enumerate() {
        payload.push(byte ^ mask[i % 4]);
    }
    cursor += payload_len;
    WsFrameParse::Complete {
        frame: WsFrame {
            fin,
            opcode,
            payload,
        },
        consumed: cursor,
    }
}

/// Decode a close frame payload: an optional 2-byte big-endian status code
/// followed by a UTF-8 reason. An absent code is reported as `1005` (RFC 6455
/// "no status received").
fn parse_ws_close_payload(payload: &[u8]) -> (u16, String) {
    if payload.len() < 2 {
        return (1005, String::new());
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    (code, String::from_utf8_lossy(&payload[2..]).into_owned())
}

/// Render the RFC 6455 `101 Switching Protocols` handshake response (ADR-0129
/// §1): the mandated `Upgrade` / `Connection` / `Sec-WebSocket-Accept`, the
/// handler's optional negotiated subprotocol, and any extra handler headers
/// (the three cap-owned handshake headers are stripped so a handler cannot
/// double them).
fn render_ws_accept(key: &str, accept: &WebSocketAccept) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut head = String::from("HTTP/1.1 101 Switching Protocols\r\n");
    head.push_str("Upgrade: websocket\r\n");
    head.push_str("Connection: Upgrade\r\n");
    let _ = write!(
        head,
        "Sec-WebSocket-Accept: {}\r\n",
        sec_websocket_accept(key)
    );
    if let Some(protocol) = &accept.subprotocol {
        let _ = write!(head, "Sec-WebSocket-Protocol: {protocol}\r\n");
    }
    for header in &accept.headers {
        if header.name.eq_ignore_ascii_case("upgrade")
            || header.name.eq_ignore_ascii_case("connection")
            || header.name.eq_ignore_ascii_case("sec-websocket-accept")
        {
            continue;
        }
        let _ = write!(head, "{}: {}\r\n", header.name, header.value);
    }
    head.push_str("\r\n");
    head.into_bytes()
}

/// Whether the reader's frame loop keeps running or stops (peer close /
/// protocol error / receiver gone).
enum WsLoop {
    Continue,
    Stop,
}

/// Act on one parsed inbound frame (ADR-0129 §4/§5), driving continuation
/// reassembly and control-frame handling. Application messages / pings / peer
/// closes post an [`InboundEvent`]; the cap (dispatcher + writer thread) does
/// all the writing — the reader only reads and reports.
#[allow(clippy::too_many_arguments)]
fn handle_ws_frame(
    frame: WsFrame,
    conn_id: ConnId,
    sink: &WakeSink,
    msg: &mut Vec<u8>,
    msg_binary: &mut bool,
    fragmenting: &mut bool,
    max_message_bytes: usize,
) -> WsLoop {
    let protocol_close = |code: u16, reason: &str| {
        sink.post(InboundEvent::WebSocketClose {
            conn_id,
            code,
            reason: reason.to_string(),
        });
        WsLoop::Stop
    };
    match frame.opcode {
        OPCODE_PING => {
            if !frame.fin {
                return protocol_close(1002, "fragmented control frame");
            }
            if sink.post(InboundEvent::WebSocketPing {
                conn_id,
                payload: frame.payload,
            }) {
                WsLoop::Continue
            } else {
                WsLoop::Stop
            }
        }
        // A pong (peer keepalive answer) is transparent — nothing to do.
        OPCODE_PONG => WsLoop::Continue,
        OPCODE_CLOSE => {
            let (code, reason) = parse_ws_close_payload(&frame.payload);
            sink.post(InboundEvent::WebSocketClose {
                conn_id,
                code,
                reason,
            });
            WsLoop::Stop
        }
        OPCODE_TEXT | OPCODE_BINARY => {
            if *fragmenting {
                return protocol_close(1002, "interleaved data frame");
            }
            let binary = frame.opcode == OPCODE_BINARY;
            if frame.fin {
                if sink.post(InboundEvent::WebSocketMessage {
                    conn_id,
                    binary,
                    data: frame.payload,
                }) {
                    WsLoop::Continue
                } else {
                    WsLoop::Stop
                }
            } else {
                *msg = frame.payload;
                *msg_binary = binary;
                *fragmenting = true;
                WsLoop::Continue
            }
        }
        OPCODE_CONTINUATION => {
            if !*fragmenting {
                return protocol_close(1002, "unexpected continuation frame");
            }
            if msg.len() + frame.payload.len() > max_message_bytes {
                return protocol_close(1009, "message too large");
            }
            msg.extend_from_slice(&frame.payload);
            if frame.fin {
                *fragmenting = false;
                let data = mem::take(msg);
                if sink.post(InboundEvent::WebSocketMessage {
                    conn_id,
                    binary: *msg_binary,
                    data,
                }) {
                    WsLoop::Continue
                } else {
                    WsLoop::Stop
                }
            } else {
                WsLoop::Continue
            }
        }
        _ => protocol_close(1002, "unknown opcode"),
    }
}

/// The websocket frame loop (ADR-0129 §4): once upgraded, the reader reads RFC
/// 6455 frames under the idle deadline, reassembles continuation frames into
/// complete messages, and posts each application message / ping / peer close as
/// an [`InboundEvent`]. It writes nothing — every outbound byte (pong, close
/// echo, and the handler's messages) goes through the single writer thread, so
/// no two writers race on the shared fd. `leftover` carries any bytes over-read
/// past the handshake head. Exits on peer close / protocol error / EOF / idle
/// timeout, posting a [`InboundEvent::WebSocketClose`] so the dispatcher tears
/// the connection down.
fn run_ws_reader_loop(
    stream: &mut TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    sink: &WakeSink,
    leftover: Vec<u8>,
    tuning: ReaderTuning,
) {
    let ReaderTuning {
        ws_idle_timeout,
        ws_max_message_bytes,
        ..
    } = tuning;
    // An idle websocket is normal — read under the (longer) idle deadline.
    let _ = stream.set_read_timeout(Some(ws_idle_timeout));
    let mut buf = leftover;
    let mut msg: Vec<u8> = Vec::new();
    let mut msg_binary = false;
    let mut fragmenting = false;
    let mut chunk = [0u8; 8 * 1024];

    loop {
        // Drain every whole frame the buffer already holds before reading more.
        loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match parse_ws_frame(&buf, ws_max_message_bytes) {
                WsFrameParse::Complete { frame, consumed } => {
                    buf.drain(..consumed);
                    if matches!(
                        handle_ws_frame(
                            frame,
                            conn_id,
                            sink,
                            &mut msg,
                            &mut msg_binary,
                            &mut fragmenting,
                            ws_max_message_bytes,
                        ),
                        WsLoop::Stop
                    ) {
                        return;
                    }
                }
                WsFrameParse::NeedMore => break,
                WsFrameParse::Error { code, reason } => {
                    sink.post(InboundEvent::WebSocketClose {
                        conn_id,
                        code,
                        reason: reason.to_string(),
                    });
                    return;
                }
            }
        }
        match read_more(stream, &mut chunk) {
            ReadStep::Filled(n) => buf.extend_from_slice(&chunk[..n]),
            ReadStep::Eof => {
                sink.post(InboundEvent::WebSocketClose {
                    conn_id,
                    code: 1006,
                    reason: "peer disconnected".to_string(),
                });
                return;
            }
            ReadStep::Timeout => {
                sink.post(InboundEvent::WebSocketClose {
                    conn_id,
                    code: 1001,
                    reason: "idle timeout".to_string(),
                });
                return;
            }
            ReadStep::Error(reason) => {
                sink.post(InboundEvent::WebSocketClose {
                    conn_id,
                    code: 1006,
                    reason,
                });
                return;
            }
        }
    }
}

/// Map a raw HTTP method token to the typed [`HttpMethod`]; `None` for a
/// non-enumerated verb (answered `501` before any dispatch).
fn parse_http_method(method: &str) -> Option<HttpMethod> {
    match method {
        "GET" => Some(HttpMethod::Get),
        "POST" => Some(HttpMethod::Post),
        "PUT" => Some(HttpMethod::Put),
        "DELETE" => Some(HttpMethod::Delete),
        "PATCH" => Some(HttpMethod::Patch),
        "HEAD" => Some(HttpMethod::Head),
        "OPTIONS" => Some(HttpMethod::Options),
        _ => None,
    }
}

/// HTTP reason phrase for the status codes the cap emits.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        426 => "Upgrade Required",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Format `now` as an RFC 7231 IMF-fixdate (`Sun, 06 Nov 1994 08:49:37
/// GMT`) for the `Date` response header. Pure integer arithmetic
/// (Howard Hinnant's civil-from-days) — no date crate.
fn http_date(now: SystemTime) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let total = i64::try_from(secs).unwrap_or(i64::MAX);
    let days = total.div_euclid(86_400);
    let rem = total.rem_euclid(86_400);
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let weekday = (days + 4).rem_euclid(7);
    // Civil date from days-since-epoch.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    let weekday_name = WEEKDAYS[usize::try_from(weekday).unwrap_or(0)];
    let month_name = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
    format!("{weekday_name}, {day:02} {month_name} {year:04} {hour:02}:{minute:02}:{second:02} GMT")
}

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
            ws_message_conn: HashMap::new(),
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
        // An inbound websocket message's chain settled (ADR-0129): reclaim its
        // outbound-routing entry. Not an `in_flight` request, so no `502`.
        if state.ws_message_conn.remove(&correlation).is_some() {
            return;
        }
        let Some(pending) = state.in_flight.remove(&correlation) else {
            // Already answered (the reply landed first) or never ours.
            return;
        };
        state.write_status_response(pending.conn_id, 502, "no response from handler");
        state.close_connection(pending.conn_id, "settled without response");
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
    /// `#[handler]`s ([`Self::on_response_chunk`] / [`Self::on_response_stream_end`]),
    /// keyed by their explicit `stream_id` payload — a second correlation
    /// regime living beside this one.
    /// An outbound websocket message from the handler (ADR-0129 §3). Routed by
    /// the causal chain, not the payload: the handler answers an inbound
    /// message on its chain (a wasm `ctx.send` inherits the inbound `root`), so
    /// `in_flight_root` names the originating connection through
    /// `ws_message_conn` — the fresh-root-per-message model gives the routing
    /// key without a connection id on the wire.
    ///
    /// # Agent
    /// Not user-callable — an upgraded connection's handler sends this to
    /// answer the peer; the cap frames it and drains it under the credit
    /// window.
    #[handler]
    fn on_websocket_message(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        msg: WebSocketMessage,
    ) {
        let root = ctx.in_flight_root().correlation_id;
        if let Some(&conn_id) = state.ws_message_conn.get(&root) {
            state.send_ws_message(conn_id, msg.binary, &msg.data);
        }
    }

    /// A handler-initiated websocket close (ADR-0129 §5). Routed by the causal
    /// chain like [`Self::on_websocket_message`]: the cap writes the close
    /// frame on the connection's writer and tears it down.
    ///
    /// # Agent
    /// Not user-callable — an upgraded connection's handler sends this to close
    /// the socket.
    #[handler]
    fn on_websocket_close(state: &mut Self::State, ctx: &mut NativeCtx<'_>, close: WebSocketClose) {
        let root = ctx.in_flight_root().correlation_id;
        if let Some(&conn_id) = state.ws_message_conn.get(&root) {
            state.send_ws_close(conn_id, close.code, &close.reason);
        }
    }

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

#[cfg(test)]
mod unit_tests {
    use super::{
        OPCODE_BINARY, OPCODE_CONTINUATION, OPCODE_TEXT, WsFrameParse, http_date, normalize_prefix,
        parse_http_method, parse_ws_frame, percent_decode_path, reason_phrase, request_keeps_alive,
        route_matches, sec_websocket_accept, serialize_ws_frame, sha1, validate_ws_handshake,
    };
    use crate::http::kinds::{HttpHeader, HttpMethod};
    use std::time::{Duration, UNIX_EPOCH};

    fn conn_header(value: &str) -> Vec<HttpHeader> {
        vec![HttpHeader {
            name: "Connection".to_string(),
            value: value.to_string(),
        }]
    }

    /// Tripwire: keep-alive defaulting is branch logic over the HTTP version
    /// and the `Connection` header, not a derived mirror — HTTP/1.1 keeps
    /// alive unless told to close, HTTP/1.0 closes unless told to keep alive,
    /// and an explicit token wins over the version default either way.
    #[test]
    fn keep_alive_defaults_by_version_and_connection_header() {
        // HTTP/1.1 (version 1): keep-alive by default, `close` overrides.
        assert!(request_keeps_alive(Some(1), &[]));
        assert!(!request_keeps_alive(Some(1), &conn_header("close")));
        assert!(request_keeps_alive(Some(1), &conn_header("keep-alive")));
        // HTTP/1.0 (version 0): close by default, `keep-alive` overrides.
        assert!(!request_keeps_alive(Some(0), &[]));
        assert!(request_keeps_alive(Some(0), &conn_header("keep-alive")));
        assert!(!request_keeps_alive(Some(0), &conn_header("close")));
        // Case-insensitive, and a token among comma-separated values counts.
        assert!(!request_keeps_alive(Some(1), &conn_header("Close")));
        assert!(request_keeps_alive(
            Some(0),
            &conn_header("keep-alive, Upgrade")
        ));
    }

    /// Segment-boundary semantics (ADR-0130): a prefix matches at `/`
    /// boundaries only, so `/api` never captures `/apiary`.
    #[test]
    fn route_match_is_segment_boundary() {
        assert!(route_matches("/api", "/api"));
        assert!(route_matches("/api", "/api/widgets"));
        assert!(!route_matches("/api", "/apiary"));
        assert!(!route_matches("/api", "/ap"));
        assert!(route_matches("/", "/anything"));
        assert!(route_matches("/", "/"));
    }

    /// Prefix normalization: leading `/` required, trailing slashes
    /// stripped to one canonical spelling, `/` kept as the catch-all.
    #[test]
    fn prefix_normalization() {
        assert_eq!(normalize_prefix("/api/"), Ok("/api".to_string()));
        assert_eq!(normalize_prefix("/api"), Ok("/api".to_string()));
        assert_eq!(normalize_prefix("/"), Ok("/".to_string()));
        assert_eq!(normalize_prefix("///"), Ok("/".to_string()));
        assert!(normalize_prefix("api").is_err());
        assert!(normalize_prefix("").is_err());
    }

    #[test]
    fn http_date_formats_the_rfc_example() {
        // RFC 7231 §7.1.1.1 canonical example.
        let when = UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(http_date(when), "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn known_methods_map_unknown_is_none() {
        assert_eq!(parse_http_method("GET"), Some(HttpMethod::Get));
        assert_eq!(parse_http_method("POST"), Some(HttpMethod::Post));
        assert_eq!(parse_http_method("OPTIONS"), Some(HttpMethod::Options));
        assert_eq!(parse_http_method("FROB"), None);
        assert_eq!(parse_http_method("get"), None);
    }

    #[test]
    fn reason_phrases_cover_emitted_statuses() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(411), "Length Required");
        assert_eq!(reason_phrase(413), "Payload Too Large");
        assert_eq!(reason_phrase(501), "Not Implemented");
        assert_eq!(reason_phrase(502), "Bad Gateway");
        assert_eq!(reason_phrase(503), "Service Unavailable");
        assert_eq!(reason_phrase(504), "Gateway Timeout");
    }

    #[test]
    fn percent_decode_path_decodes_valid_escapes_and_passes_through_invalid_ones() {
        assert_eq!(percent_decode_path("/hello%20world"), "/hello world");
        assert_eq!(percent_decode_path("/no-escapes"), "/no-escapes");
        // Trailing `%` / `%2` (too short for a full escape) pass through
        // literally rather than erroring.
        assert_eq!(percent_decode_path("/trailing%"), "/trailing%");
        assert_eq!(percent_decode_path("/trailing%2"), "/trailing%2");
        // Non-hex digits pass through literally.
        assert_eq!(percent_decode_path("/bad%zzescape"), "/bad%zzescape");
    }

    #[test]
    fn sha1_matches_the_rfc_3174_vector() {
        // Tripwire: RFC 3174 §7.3 worked example sha1("abc"). A computed digest
        // that drifts if the block schedule / padding / round logic breaks.
        use std::fmt::Write as _;
        let digest = sha1(b"abc");
        let hex = digest.iter().fold(String::new(), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        });
        assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn sec_websocket_accept_matches_the_rfc_6455_vector() {
        // Tripwire: RFC 6455 §1.3 worked handshake vector — base64(SHA-1(key +
        // GUID)). A computed value pinning the SHA-1, the GUID, and the base64
        // together; it drifts if any of the three is wrong (the GUID's last
        // byte especially).
        assert_eq!(
            sec_websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn ws_frame_serialize_matches_rfc_6455_examples() {
        // Tripwire: RFC 6455 §5.7 byte-layout examples — computed frame bytes.
        // #1 single unmasked text "Hello".
        assert_eq!(
            serialize_ws_frame(OPCODE_TEXT, b"Hello", None),
            vec![0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f]
        );
        // #2 single masked text "Hello" (mask 0x37fa213d).
        assert_eq!(
            serialize_ws_frame(OPCODE_TEXT, b"Hello", Some([0x37, 0xfa, 0x21, 0x3d])),
            vec![
                0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58
            ]
        );
        // #4 256-byte binary: the 16-bit extended-length header.
        let big = serialize_ws_frame(OPCODE_BINARY, &[0u8; 256], None);
        assert_eq!(&big[..4], &[0x82, 0x7e, 0x01, 0x00]);
        // #5 65536-byte binary: the 64-bit extended-length header.
        let huge = serialize_ws_frame(OPCODE_BINARY, &vec![0u8; 65_536], None);
        assert_eq!(
            &huge[..10],
            &[0x82, 0x7f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn ws_frame_parse_unmasks_the_rfc_6455_masked_example() {
        // Tripwire: parse RFC 6455 §5.7 #2 (masked "Hello") back to its
        // payload — the mask XOR and header decode as a round-trip of the
        // serialize tripwire above.
        let bytes = [
            0x81u8, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ];
        match parse_ws_frame(&bytes, 1024) {
            WsFrameParse::Complete { frame, consumed } => {
                assert!(frame.fin);
                assert_eq!(frame.opcode, OPCODE_TEXT);
                assert_eq!(frame.payload, b"Hello");
                assert_eq!(consumed, bytes.len());
            }
            _ => panic!("expected a complete frame"),
        }
    }

    #[test]
    fn ws_frame_parse_rejects_an_unmasked_client_frame() {
        // A client→server frame MUST be masked (RFC 6455 §5.1); an unmasked one
        // is a `1002` protocol error, never a panic on untrusted input.
        let bytes = [0x81u8, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
        assert!(matches!(
            parse_ws_frame(&bytes, 1024),
            WsFrameParse::Error { code: 1002, .. }
        ));
    }

    #[test]
    fn ws_frame_parse_needs_more_on_a_partial_frame() {
        // A header that announces more payload than is buffered yields NeedMore,
        // not a panic or a wrong-length read.
        let bytes = [0x81u8, 0x85, 0x37, 0xfa, 0x21]; // header cut mid-mask-key
        assert!(matches!(
            parse_ws_frame(&bytes, 1024),
            WsFrameParse::NeedMore
        ));
    }

    #[test]
    fn ws_continuation_frames_decode_as_a_fragmented_message() {
        // Tripwire: a fragmented text message "Hel" + "lo" as masked frames —
        // the first non-final (FIN clear, opcode text), the second final (FIN
        // set, opcode continuation). Field-by-field decode of masking + the FIN
        // bit + the continuation opcode; concatenating the payloads yields the
        // original message (the reassembly the frame loop performs).
        let mask = [0x01u8, 0x02, 0x03, 0x04];
        let mut first = serialize_ws_frame(OPCODE_TEXT, b"Hel", Some(mask));
        first[0] &= 0x7f; // clear FIN — a non-final fragment
        let second = serialize_ws_frame(OPCODE_CONTINUATION, b"lo", Some(mask));

        let WsFrameParse::Complete { frame: f1, .. } = parse_ws_frame(&first, 1024) else {
            panic!("first fragment must parse");
        };
        assert!(!f1.fin);
        assert_eq!(f1.opcode, OPCODE_TEXT);
        assert_eq!(f1.payload, b"Hel");

        let WsFrameParse::Complete { frame: f2, .. } = parse_ws_frame(&second, 1024) else {
            panic!("continuation fragment must parse");
        };
        assert!(f2.fin);
        assert_eq!(f2.opcode, OPCODE_CONTINUATION);
        assert_eq!(f2.payload, b"lo");

        let mut message = f1.payload;
        message.extend_from_slice(&f2.payload);
        assert_eq!(message, b"Hello");
    }

    #[test]
    fn ws_handshake_validation_enforces_version_and_key() {
        let base = |extra: &[(&str, &str)]| -> Vec<HttpHeader> {
            let mut headers = vec![HttpHeader {
                name: "Connection".to_string(),
                value: "Upgrade".to_string(),
            }];
            for (name, value) in extra {
                headers.push(HttpHeader {
                    name: (*name).to_string(),
                    value: (*value).to_string(),
                });
            }
            headers
        };
        // Valid: version 13 + a key echoes the key back.
        assert_eq!(
            validate_ws_handshake(&base(&[
                ("Sec-WebSocket-Version", "13"),
                ("Sec-WebSocket-Key", "abc"),
            ])),
            Ok("abc".to_string())
        );
        // Wrong version → 426.
        assert!(matches!(
            validate_ws_handshake(&base(&[
                ("Sec-WebSocket-Version", "8"),
                ("Sec-WebSocket-Key", "abc"),
            ])),
            Err((426, _))
        ));
        // Missing key → 400.
        assert!(matches!(
            validate_ws_handshake(&base(&[("Sec-WebSocket-Version", "13")])),
            Err((400, _))
        ));
        // Missing Connection: Upgrade → 400.
        assert!(matches!(
            validate_ws_handshake(&[
                HttpHeader {
                    name: "Sec-WebSocket-Version".to_string(),
                    value: "13".to_string(),
                },
                HttpHeader {
                    name: "Sec-WebSocket-Key".to_string(),
                    value: "abc".to_string(),
                },
            ]),
            Err((400, _))
        ));
    }

    #[test]
    fn config_layer_defaults_match_the_named_consts() {
        use super::super::{
            DEFAULT_BIND_ADDR, DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS, DEFAULT_MAX_CONNECTIONS,
            DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_REQUEST_BYTES, DEFAULT_REQUEST_STREAM_WINDOW,
            DEFAULT_REQUEST_TIMEOUT_MILLIS, DEFAULT_RESPONSE_STREAM_WINDOW,
            DEFAULT_WS_IDLE_TIMEOUT_MILLIS, HttpServerConfig, HttpServerConfigLayer,
        };
        use confique::Config as _;
        // No `.env()` source: loads the literal defaults only, so this is
        // env-free and guards the layer defaults against the consts +
        // `HttpServerConfig::default()`.
        let layer = HttpServerConfigLayer::builder()
            .load()
            .expect("defaults load");
        let default = HttpServerConfig::default();
        assert_eq!(layer.bind_addr, DEFAULT_BIND_ADDR);
        assert_eq!(layer.bind_addr, default.bind_addr);
        assert_eq!(layer.handler_mailbox, "");
        assert_eq!(layer.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
        assert_eq!(layer.max_header_bytes, DEFAULT_MAX_HEADER_BYTES);
        assert_eq!(layer.request_timeout_millis, DEFAULT_REQUEST_TIMEOUT_MILLIS);
        assert_eq!(
            layer.keep_alive_timeout_millis,
            DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS
        );
        assert_eq!(
            default.keep_alive_timeout_millis,
            DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS
        );
        assert_eq!(layer.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(layer.max_connections, default.max_connections);
        assert_eq!(layer.response_stream_window, DEFAULT_RESPONSE_STREAM_WINDOW);
        assert_eq!(
            default.response_stream_window,
            DEFAULT_RESPONSE_STREAM_WINDOW
        );
        assert_eq!(layer.request_stream_window, DEFAULT_REQUEST_STREAM_WINDOW);
        assert_eq!(default.request_stream_window, DEFAULT_REQUEST_STREAM_WINDOW);
        assert_eq!(
            layer.websocket_idle_timeout_millis,
            DEFAULT_WS_IDLE_TIMEOUT_MILLIS
        );
        assert_eq!(
            default.websocket_idle_timeout_millis,
            DEFAULT_WS_IDLE_TIMEOUT_MILLIS
        );
    }
}
