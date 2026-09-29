//! Durable stream handles for the HTTP server data phase (ADR-0133).
//!
//! The handshake legs of a streamed or websocket response are `ctx.reply`s
//! that route to whoever dispatched the request. These handles extend the
//! same invariant to the data phase: a handler answers the counterparty
//! that dispatched to it — the real [`HttpServerCapability`], a test mock,
//! or a middleware forwarding in front of the cap — never a compile-time
//! singleton.
//!
//! A handle is plain data: the `counterparty` [`ProtocolRef`] — the proven
//! sender of the dispatch that opened the stream (`ctx.sender()`, ADR-0230),
//! cast once to the sink protocol the handle emits (`ctx.cast`, ADR-0231 §4)
//! — plus the `stream_id` naming the connection. The sinks are
//! [`ResponseSink`], [`RequestCreditSink`], and [`WebSocketSink`]: each names
//! the silent rows its handle sends, so an emit compiles only for those
//! kinds, and the cast refuses a sender that would warn-drop one. The
//! counterparty is still whoever dispatched, so a mock or a middleware that
//! covers the sink stands in for the cap. A handle is constructed once (from
//! the first credit grant, or the request-stream open), stored on the
//! handler, and used from later handlers to emit the stream. Every send is a
//! detached chain root at the stored counterparty
//! ([`MailSender::send_detached_to`]): the data-phase mails are per-message
//! causal chains (ADR-0128 / ADR-0132), so a chunk is attributed to its own
//! root rather than to whatever handler happened to emit it, and a handler
//! can push unprompted from any chain.
//!
//! Wasm-safe like [`super::typed`]: it names only `kinds.rs` payloads and
//! the `aether-actor` model traits and references, so a
//! `default-features = false` guest gets it without the native runtime.
//!
//! [`HttpServerCapability`]: super::HttpServerCapability

use aether_actor::{MailSender, ProtocolRef};

use super::kinds::{
    HttpRequestCredit, HttpRequestStreamOpen, HttpResponseChunk, HttpResponseStreamEnd, HttpStreamCredit,
    WebSocketClose, WebSocketMessage,
};

/// What a [`ResponseStream`] sends its counterparty (ADR-0128): the body
/// chunks and the terminator, both silent, as the dispatch shard receives
/// them.
#[aether_actor::protocol]
pub trait ResponseSink {
    /// One body chunk.
    fn chunk(mail: HttpResponseChunk);
    /// The stream's terminator.
    fn end(mail: HttpResponseStreamEnd);
}

/// What a [`RequestStream`] sends its counterparty (ADR-0128): the silent
/// read-credit grants that pace a streamed upload.
#[aether_actor::protocol]
pub trait RequestCreditSink {
    /// One read-credit grant.
    fn credit(mail: HttpRequestCredit);
}

/// What a [`WebSocketStream`] sends its counterparty (ADR-0129 / ADR-0132):
/// the outbound messages and the close, both silent.
#[aether_actor::protocol]
pub trait WebSocketSink {
    /// One application message to the peer.
    fn message(mail: WebSocketMessage);
    /// The close handshake's initiation.
    fn close(mail: WebSocketClose);
}

/// A streamed response a handler is feeding (ADR-0128 / ADR-0133): the
/// counterparty that opened the stream plus the `stream_id` naming it.
/// Constructed from the first [`HttpStreamCredit`] grant, then used from
/// the credit handler to emit body chunks and the terminator.
#[derive(Debug, Clone, Copy)]
pub struct ResponseStream {
    /// The proven sender that dispatched the opening credit — the cap, a
    /// mock, or a middleware — cast to the [`ResponseSink`] it covers. Every
    /// send on this handle targets it.
    pub counterparty: ProtocolRef<ResponseSink>,
    /// The stream id the cap assigned this response (ADR-0128), stamped on
    /// every chunk and the terminator.
    pub stream_id: u64,
}

impl ResponseStream {
    /// Capture the handle from the first [`HttpStreamCredit`] the cap
    /// dispatched. `counterparty` is the credit handler's `ctx.sender()`
    /// cast to a [`ResponseSink`], so a handle cannot exist without the proof
    /// it sends to. A sourceless dispatch (broadcast / session /
    /// substrate-origin mail) yields no sender, and a sender that does not
    /// cover the sink yields no cast, so neither yields a handle — the call
    /// site decides, and a handler stores an `Option<ResponseStream>` that
    /// stays `None` until the first grant arms it.
    #[must_use]
    pub fn from_credit(counterparty: ProtocolRef<ResponseSink>, credit: &HttpStreamCredit) -> Self {
        Self { counterparty, stream_id: credit.stream_id }
    }

    /// Emit one body chunk on this stream (ADR-0128 [`HttpResponseChunk`]),
    /// a detached root at the stored counterparty.
    pub fn chunk(&self, ctx: &mut impl MailSender, body: Vec<u8>) {
        ctx.send_detached_to(self.counterparty, &HttpResponseChunk { stream_id: self.stream_id, body });
    }

    /// Terminate this stream (ADR-0128 [`HttpResponseStreamEnd`]). The cap
    /// writes the terminating zero-length chunk and closes the connection.
    pub fn end(&self, ctx: &mut impl MailSender) {
        ctx.send_detached_to(self.counterparty, &HttpResponseStreamEnd { stream_id: self.stream_id });
    }
}

/// A streamed request a handler is draining (ADR-0128 / ADR-0133): the
/// counterparty that opened the stream plus its `stream_id`. Constructed
/// from the [`HttpRequestStreamOpen`] the cap dispatches when a streamed
/// upload begins, then used to grant read credit back as chunks drain.
#[derive(Debug, Clone, Copy)]
pub struct RequestStream {
    /// The proven sender that opened the request stream, cast to the
    /// [`RequestCreditSink`] it covers — every credit grant on this handle
    /// targets it.
    pub counterparty: ProtocolRef<RequestCreditSink>,
    /// The stream id the cap assigned this upload (ADR-0128), stamped on
    /// every credit grant.
    pub stream_id: u64,
}

impl RequestStream {
    /// Capture the handle from the [`HttpRequestStreamOpen`] that begins a
    /// streamed upload. `counterparty` is the open handler's `ctx.sender()`
    /// cast to a [`RequestCreditSink`], and a sourceless dispatch or a sender
    /// that does not cover the sink yields none — the same guard as
    /// [`ResponseStream::from_credit`], decided at the call site.
    #[must_use]
    pub fn from_open(counterparty: ProtocolRef<RequestCreditSink>, open: &HttpRequestStreamOpen) -> Self {
        Self { counterparty, stream_id: open.stream_id }
    }

    /// Grant the cap credit to deliver up to `credit` more inbound chunks
    /// (ADR-0128 [`HttpRequestCredit`]), a detached root at the stored
    /// counterparty.
    pub fn credit(&self, ctx: &mut impl MailSender, credit: u32) {
        ctx.send_detached_to(self.counterparty, &HttpRequestCredit { stream_id: self.stream_id, credit });
    }
}

/// An upgraded websocket connection (ADR-0129 / ADR-0132 / ADR-0133): the
/// counterparty that owns the socket plus its `stream_id`. Constructed
/// from the first [`HttpStreamCredit`] grant (the accept-time window), then
/// used to push messages and initiate a close from any chain.
#[derive(Debug, Clone, Copy)]
pub struct WebSocketStream {
    /// The proven sender that owns the upgraded connection, cast to the
    /// [`WebSocketSink`] it covers — every outbound message and close on
    /// this handle targets it.
    pub counterparty: ProtocolRef<WebSocketSink>,
    /// The connection's stream id (ADR-0132), stamped on every outbound
    /// message and close.
    pub stream_id: u64,
}

impl WebSocketStream {
    /// Capture the handle from the first [`HttpStreamCredit`] grant, which
    /// the cap sends at accept time before any peer traffic (ADR-0132).
    /// `counterparty` is that handler's `ctx.sender()` cast to a
    /// [`WebSocketSink`]; a sourceless dispatch or a sender that does not
    /// cover the sink yields none, so the call site decides.
    #[must_use]
    pub fn from_credit(counterparty: ProtocolRef<WebSocketSink>, credit: &HttpStreamCredit) -> Self {
        Self { counterparty, stream_id: credit.stream_id }
    }

    /// Push one application message to the peer (ADR-0132
    /// [`WebSocketMessage`]), a detached root at the stored counterparty.
    /// `binary` selects the RFC 6455 opcode.
    pub fn message(&self, ctx: &mut impl MailSender, binary: bool, data: Vec<u8>) {
        ctx.send_detached_to(self.counterparty, &WebSocketMessage { stream_id: self.stream_id, binary, data });
    }

    /// Initiate the close handshake (ADR-0129 [`WebSocketClose`]). `code` is
    /// the RFC 6455 close status; `reason` the optional UTF-8 phrase.
    pub fn close(&self, ctx: &mut impl MailSender, code: u16, reason: String) {
        ctx.send_detached_to(self.counterparty, &WebSocketClose { stream_id: self.stream_id, code, reason });
    }
}
