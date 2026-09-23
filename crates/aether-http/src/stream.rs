//! Durable stream handles for the HTTP server data phase (ADR-0133).
//!
//! The handshake legs of a streamed or websocket response are `ctx.reply`s
//! that route to whoever dispatched the request. These handles extend the
//! same invariant to the data phase: a handler answers the counterparty
//! that dispatched to it — the real [`HttpServerCapability`], a test mock,
//! or a middleware forwarding in front of the cap — never a compile-time
//! singleton.
//!
//! A handle is plain data: the `counterparty` [`ErasedActorRef`] — the proven
//! sender of the dispatch that opened the stream, minted by `ctx.sender()`
//! (ADR-0230) — plus the `stream_id` naming the connection. It is
//! constructed once (from the first credit grant, or the request-stream
//! open), stored on the handler, and used from later handlers to emit the
//! stream. Every send is a detached chain root at the stored counterparty
//! ([`MailSender::send_detached_to`]): the data-phase mails are per-message
//! causal chains (ADR-0128 / ADR-0132), so a chunk is attributed to its own
//! root rather than to whatever handler happened to emit it, and a handler
//! can push unprompted from any chain.
//!
//! Wasm-safe like [`super::typed`]: it names only `kinds.rs` payloads and
//! the `aether-actor` model traits, so a `default-features = false` guest
//! gets it without the native runtime.
//!
//! [`HttpServerCapability`]: super::HttpServerCapability

use aether_actor::{ErasedActorRef, MailSender};

use super::kinds::{
    HttpRequestCredit, HttpRequestStreamOpen, HttpResponseChunk, HttpResponseStreamEnd, HttpStreamCredit,
    WebSocketClose, WebSocketMessage,
};

/// A streamed response a handler is feeding (ADR-0128 / ADR-0133): the
/// counterparty that opened the stream plus the `stream_id` naming it.
/// Constructed from the first [`HttpStreamCredit`] grant, then used from
/// the credit handler to emit body chunks and the terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseStream {
    /// The proven sender that dispatched the opening credit — the cap, a
    /// mock, or a middleware. Every send on this handle targets it.
    pub counterparty: ErasedActorRef,
    /// The stream id the cap assigned this response (ADR-0128), stamped on
    /// every chunk and the terminator.
    pub stream_id: u64,
}

impl ResponseStream {
    /// Capture the handle from the first [`HttpStreamCredit`] the cap
    /// dispatched. `sender` is the credit handler's `ctx.sender()`, so a
    /// handle cannot exist without the proof it sends to. A sourceless
    /// dispatch (broadcast / session / substrate-origin mail) yields no
    /// sender and therefore no handle — the call site decides, and a
    /// handler stores an `Option<ResponseStream>` that stays `None` until
    /// the first grant arms it.
    #[must_use]
    pub fn from_credit(sender: ErasedActorRef, credit: &HttpStreamCredit) -> Self {
        Self { counterparty: sender, stream_id: credit.stream_id }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestStream {
    /// The proven sender that opened the request stream — every credit
    /// grant on this handle targets it.
    pub counterparty: ErasedActorRef,
    /// The stream id the cap assigned this upload (ADR-0128), stamped on
    /// every credit grant.
    pub stream_id: u64,
}

impl RequestStream {
    /// Capture the handle from the [`HttpRequestStreamOpen`] that begins a
    /// streamed upload. `sender` is the open handler's `ctx.sender()`, and a
    /// sourceless dispatch yields none — the same guard as
    /// [`ResponseStream::from_credit`], decided at the call site.
    #[must_use]
    pub fn from_open(sender: ErasedActorRef, open: &HttpRequestStreamOpen) -> Self {
        Self { counterparty: sender, stream_id: open.stream_id }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketStream {
    /// The proven sender that owns the upgraded connection — every outbound
    /// message and close on this handle targets it.
    pub counterparty: ErasedActorRef,
    /// The connection's stream id (ADR-0132), stamped on every outbound
    /// message and close.
    pub stream_id: u64,
}

impl WebSocketStream {
    /// Capture the handle from the first [`HttpStreamCredit`] grant, which
    /// the cap sends at accept time before any peer traffic (ADR-0132).
    /// `sender` is that handler's `ctx.sender()`; a sourceless dispatch
    /// yields none, so the call site decides.
    #[must_use]
    pub fn from_credit(sender: ErasedActorRef, credit: &HttpStreamCredit) -> Self {
        Self { counterparty: sender, stream_id: credit.stream_id }
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
