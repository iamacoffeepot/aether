// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

/// Per-connection identifier, monotonic within this cap. Distinct from
/// the OS-level peer addr (one peer may reconnect; ids stay unique for
/// the cap's lifetime).
pub type ConnId = u64;

/// Header-array size for the inbound parse (doubles as the header-count
/// cap: a request with more headers is answered `431`). ADR-0108 §6.
pub const MAX_HEADER_COUNT: usize = 64;

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
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
    /// Whether this request wants the connection kept alive after its
    /// response (HTTP/1.1 default, or HTTP/1.0 with `Connection:
    /// keep-alive`), computed by [`request_keeps_alive`]. Carried to the
    /// dispatcher so it renders `Connection: keep-alive` vs `close` and
    /// either resumes the reader or closes the socket.
    pub keep_alive: bool,
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
pub enum WriterMsg {
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
    pub peer: SocketAddr,
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
    pub conn_id: ConnId,
    /// The handler mailbox this stream's credit grants address. For a response
    /// stream (ADR-0128) it is the late-bound `handler_mailbox`; for a
    /// websocket (ADR-0129) it is the handler resolved at handshake. Stored so
    /// credit replenishment addresses the right actor without a re-lookup.
    pub handler: MailboxId,
    /// Bounded hand-off to the writer thread. `try_send` never blocks the
    /// dispatcher: the credit accounting keeps the invariant
    /// `credit_outstanding + queued <= window`, so a slot is always free when
    /// a within-credit chunk arrives.
    pub tx: mpsc::SyncSender<WriterMsg>,
    /// Writer thread handle. Detached on teardown / close (the dispatcher
    /// must never block joining a slow-peer write); joined in `unwire` after
    /// the sender is dropped.
    pub writer_thread: Option<JoinHandle<()>>,
    /// Credits granted to the handler it has not yet spent, bounded by the
    /// window. A chunk arriving with this at zero is an over-window flood
    /// (ADR-0128 §Consequences trust boundary) → the stream is torn down.
    pub credit_outstanding: u32,
    /// The handler sent `HttpResponseStreamEnd` but the terminator did not
    /// fit the bounded channel yet (chunks still queued). Flushed as slots
    /// free so the terminating chunk always follows the body in order.
    pub pending_end: bool,
    /// Whether the connection is kept alive once this stream finishes
    /// (renders `Connection: keep-alive` in the stream head and resumes the
    /// reader at [`HttpServerCapabilityState::finish_stream`]) rather than
    /// closing it. Set from the promoted request's [`PendingRequest`] at
    /// stream open, mirroring `PendingRequest::keep_alive`.
    pub keep_alive: bool,
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
    pub conn_id: ConnId,
    /// The resolved handler mailbox the cap delivers `HttpRequestChunk` /
    /// `HttpRequestStreamEnd` to. Captured at stream open so mid-stream
    /// delivery skips route re-resolution.
    pub handler: MailboxId,
    /// The request method, carried to the final response's [`PendingRequest`]
    /// so a HEAD response suppresses its body.
    pub method: HttpMethod,
    /// Whether the final response keeps the connection alive, carried to the
    /// final response's [`PendingRequest`].
    pub keep_alive: bool,
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
