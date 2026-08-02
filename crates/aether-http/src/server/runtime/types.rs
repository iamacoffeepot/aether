// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

/// Per-connection identifier, monotonic within one dispatch shard.
/// Distinct from the OS-level peer addr (one peer may reconnect; ids stay
/// unique for the shard's lifetime).
pub type ConnId = u64;

/// The route table (ADR-0130) shared across the supervisor, its dispatch
/// shards, and (in later ADR-0135 stages) the reader threads: the
/// supervisor's registration handlers write, request dispatch reads.
pub type SharedRoutes = Arc<RwLock<Vec<Route>>>;

/// Read-mostly state a reader thread consults to make the fast-path
/// decision itself (ADR-0135 §2): the shared route table and the
/// connection's peer string (captured once at adoption; every request's
/// `peer_addr` clones it on the reader thread rather than the shard).
pub struct ReaderShared {
    pub routes: SharedRoutes,
    pub peer: String,
}

/// Boot config the supervisor builds for each dispatch shard it spawns
/// (ADR-0135): the shard's sidecar channel (receiver consumed at `init`,
/// sender cloned into every reader the shard spawns), the shared route
/// table + live-connection count, and the per-connection tuning copied
/// from [`HttpServerConfig`].
pub struct HttpShardSeed {
    /// The shard's inbound event channel; `Some` exactly until the shard's
    /// `init` takes it (the `TcpSessionConfig::stream` consume pattern).
    pub inbound_rx: Option<mpsc::Receiver<InboundEvent>>,
    /// The matching sender: the supervisor keeps a clone (its assignment
    /// sink), and the shard clones it into each reader's [`WakeSink`].
    pub inbound_tx: mpsc::Sender<InboundEvent>,
    /// The shard's wake-coalescing flag (ADR-0135 §4), shared between
    /// the supervisor's assignment sink and every sink the shard builds.
    pub wake_dirty: Arc<AtomicBool>,
    pub routes: SharedRoutes,
    pub live_connections: Arc<AtomicUsize>,
    pub max_request_bytes: usize,
    pub max_header_bytes: usize,
    pub request_timeout: Duration,
    pub keep_alive_timeout: Duration,
    pub ws_idle_timeout: Duration,
    pub response_stream_window: u32,
    pub request_stream_window: u32,
    /// Cap-global stream-id source (ADR-0135) — see
    /// [`HttpShardState::next_stream_id`].
    pub next_stream_id: Arc<AtomicU64>,
}

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
    /// Stream the body incrementally, seeded with `credit` initial send-window
    /// units (one per [`InboundEvent::RequestBodyChunk`]).
    Stream { credit: u32 },
    /// Replenish the streaming reader's send window by `credit` — the handler
    /// drained chunks and granted more.
    Credit { credit: u32 },
    /// The rendered response bytes for the request this reader has in
    /// flight (ADR-0135 §3): the reader writes them on its own socket
    /// clone — the write syscall never runs on the shard's dispatch —
    /// then loops into the next request (`resume: true`, keep-alive) or
    /// exits posting `ReaderClosed` (`resume: false`, close-mode and
    /// every canned terminal status).
    Respond { bytes: Vec<u8>, resume: bool },
    /// Keep-alive resume with nothing to write: a streamed response
    /// finished on the writer thread (ADR-0128); read the next request
    /// on the same socket.
    Resume,
    /// Websocket upgrade accepted (ADR-0129): the `101` was written and the
    /// connection is now in websocket mode. The parked reader leaves the
    /// HTTP request lifecycle and enters the RFC 6455 frame loop, reading
    /// under the websocket idle deadline (not `request_timeout`), carrying
    /// forward any bytes it over-read past the handshake head.
    Upgrade,
}

/// Internal event the accept / reader sidecar threads push to the cap
/// dispatcher via an mpsc. The matching wake-mail kind is
/// [`HttpInboundReady`] (empty payload) — `on_inbound_ready` drains the
/// channel and acts per item.
pub enum InboundEvent {
    /// The accept thread took a new connection.
    PeerAccepted { stream: TcpStream, peer: SocketAddr },
    /// A reader parsed a request head bound for a *streaming* handler
    /// (ADR-0128 / ADR-0135 §2): the reader already resolved `handler`
    /// and read its accept-set; the shard opens the inbound request
    /// stream and replies [`ReaderControl::Stream`] down the control
    /// channel. Buffered requests never take this round trip.
    RequestHeadParsed { conn_id: ConnId, head: ParsedHead, handler: MailboxId },
    /// A complete, size-bounded buffered request, resolved and encoded
    /// at the reader (ADR-0135 §2): `payload` is the ready-to-send
    /// `HttpServerRequest` (or routed-kind) wire image; the shard's
    /// dispatch is send + settlement-subscribe + in-flight insert.
    RequestParsed {
        conn_id: ConnId,
        payload: Vec<u8>,
        handler: MailboxId,
        kind: KindId,
        method: HttpMethod,
        keep_alive: bool,
        /// `Some` on a websocket upgrade handshake (ADR-0129) the
        /// reader validated: the `Sec-WebSocket-Key` the shard stashes
        /// before dispatching, consumed if the handler accepts.
        ws_key: Option<String>,
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
    WebSocketMessage { conn_id: ConnId, binary: bool, data: Vec<u8> },
    /// A websocket reader received a ping frame (ADR-0129); the dispatcher
    /// answers it with a pong on the writer thread, transparently — the ping
    /// never reaches the handler. Carries the ping payload the pong echoes.
    WebSocketPing { conn_id: ConnId, payload: Vec<u8> },
    /// A websocket reader received a peer close frame, or hit a protocol error
    /// / oversize frame / idle timeout on the upgraded socket (ADR-0129). The
    /// dispatcher reports a [`WebSocketClose`] to the handler, echoes the close
    /// frame on the writer, and tears the connection down.
    WebSocketClose { conn_id: ConnId, code: u16, reason: String },
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
    /// the dispatcher's streaming go-ahead ([`ReaderControl::Stream`] — a
    /// buffered request never takes that round trip), mid-stream credit
    /// replenishment ([`ReaderControl::Credit`]), and the keep-alive resume
    /// signal ([`ReaderControl::Resume`]) to the parked reader. Dropping this
    /// sender (on [`HttpShardState::close_connection`]) makes the reader's
    /// `recv_timeout` return `Disconnected`, its close-path exit.
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
/// `stream_id` in [`HttpShardState::streams`]; `handler` is the
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
/// always dispatches via `send_envelope_detached`).
#[derive(Copy, Clone)]
pub struct PendingRequest {
    pub conn_id: ConnId,
    /// The request's method, carried so the reply path can suppress the
    /// body on a HEAD response (message-body semantics forbid one).
    pub method: HttpMethod,
    /// Whether the reply path keeps the connection alive (renders
    /// `Connection: keep-alive` and resumes the reader) rather than
    /// closing it. Set from the [`ParsedHead`] at dispatch.
    pub keep_alive: bool,
    /// The handler this request dispatched to. Carried so a `WebSocketAccept`
    /// reply (ADR-0129) resolves the same handler for the upgraded
    /// connection's inbound dispatch + credit grants without re-resolving.
    pub handler: MailboxId,
}

/// Per-connection response-stream state (ADR-0128), keyed in
/// [`HttpShardState::streams`] by the cap-minted `stream_id` the stream was
/// opened with. The dispatcher owns all of this single-threaded, exactly like
/// [`PendingRequest`]; the writer thread owns only the socket write.
pub struct StreamState {
    /// The connection this stream writes to. Teardown paths locate a stream
    /// by connection through this field.
    pub conn_id: ConnId,
    /// The handler mailbox this stream's credit grants address. For a response
    /// stream (ADR-0128) it is the registrant of the matched route (ADR-0130);
    /// for a websocket (ADR-0129) it is the handler resolved at handshake. Stored
    /// so credit replenishment addresses the right actor without a re-lookup.
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
    /// The handler sent `HttpResponseStreamEnd`; no more chunks are coming,
    /// so no further credit is ever granted. Distinct from
    /// [`Self::pending_end`], which clears once the terminator is handed to
    /// the writer: a slot freed by a still-draining body chunk after that
    /// hand-off must not mint a grant, because the handler is done with this
    /// stream and the stale grant would leak into its *next* request's
    /// stream accounting (issue 3797).
    pub ended: bool,
    /// The handler sent `HttpResponseStreamEnd` but the terminator did not
    /// fit the bounded channel yet (chunks still queued). Flushed as slots
    /// free so the terminating chunk always follows the body in order.
    pub pending_end: bool,
    /// Whether the connection is kept alive once this stream finishes
    /// (renders `Connection: keep-alive` in the stream head and resumes the
    /// reader at [`HttpShardState::finish_stream`]) rather than
    /// closing it. Set from the promoted request's [`PendingRequest`] at
    /// stream open, mirroring `PendingRequest::keep_alive`.
    pub keep_alive: bool,
}

/// Per-connection *inbound* request-stream state (ADR-0128), keyed in
/// [`HttpShardState::request_streams`] by a cap-minted `stream_id`.
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
///
/// The wake mail coalesces behind `dirty` (ADR-0135 §4): every sink
/// targeting one drain loop shares the same flag, a post only fires the
/// wake when it flips the flag clear → set, and the drain handler clears
/// the flag *before* draining ([`WakeSink::arm_for_drain`]). A burst of
/// events costs one wake instead of one per event; an event posted
/// mid-drain re-arms the flag, so the follow-up wake is at most one
/// spurious drain, never a lost event. The clear-before-drain /
/// send-before-swap order is the load-bearing part of that guarantee.
pub struct WakeSink {
    pub inbound_tx: mpsc::Sender<InboundEvent>,
    pub mailer: Arc<Mailer>,
    pub self_id: MailboxId,
    pub wake_kind: KindId,
    /// Shared per-drain-target coalescing flag: `true` while a fired
    /// wake mail has not yet begun its drain.
    pub dirty: Arc<AtomicBool>,
}

impl WakeSink {
    /// Post one event, waking the drain loop only if no wake is already
    /// pending for it. Returns `false` when the receiver is gone (the
    /// cap tore down) so the caller stops.
    pub fn post(&self, event: InboundEvent) -> bool {
        if self.inbound_tx.send(event).is_err() {
            return false;
        }
        if !self.dirty.swap(true, Ordering::AcqRel) {
            self.mailer.push(Mail::new(
                self.self_id,
                self.wake_kind,
                HttpInboundReady::default().encode_into_bytes(),
                1,
            ));
        }
        true
    }

    /// Drain-side arm: clear the coalescing flag. Must run *before* the
    /// first `try_recv` of the drain loop — an event posted after the
    /// clear re-fires the wake, an event posted before it is already in
    /// the channel this drain is about to empty.
    pub fn arm_for_drain(dirty: &AtomicBool) {
        dirty.store(false, Ordering::Release);
    }
}
