//! Durable stream handles for the HTTP server data phase (ADR-0133).
//!
//! The handshake legs of a streamed or websocket response are `ctx.reply`s
//! that route to whoever dispatched the request. These handles extend the
//! same invariant to the data phase: a handler answers the counterparty
//! that dispatched to it — the real [`HttpServerCapability`], a test mock,
//! or a middleware forwarding in front of the cap — never a compile-time
//! singleton.
//!
//! A handle is plain data: the `counterparty` [`MailboxId`] captured from
//! the dispatch that opened the stream, plus the `stream_id` naming the
//! connection. It is constructed once (from the first credit grant, or the
//! request-stream open), stored on the handler, and used from later
//! handlers to emit the stream. Every send is a detached chain root at the
//! stored counterparty ([`MailSender::send_detached_to`]): the data-phase
//! mails are per-message causal chains (ADR-0128 / ADR-0132), so a chunk
//! is attributed to its own root rather than to whatever handler happened
//! to emit it, and a handler can push unprompted from any chain.
//!
//! Wasm-safe like [`super::typed`]: it names only `kinds.rs` payloads and
//! the `aether-actor` model traits, so a `default-features = false` guest
//! gets it without the native runtime.
//!
//! [`HttpServerCapability`]: super::HttpServerCapability
//! [`MailboxId`]: MailboxId

use aether_actor::{MailSender, OutboundReply};
use aether_data::MailboxId;

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
    /// The mailbox that dispatched the opening credit — the cap, a mock, or
    /// a middleware. Every send on this handle targets it.
    pub counterparty: MailboxId,
    /// The stream id the cap assigned this response (ADR-0128), stamped on
    /// every chunk and the terminator.
    pub stream_id: u64,
}

impl ResponseStream {
    /// Capture the handle from the first [`HttpStreamCredit`] the cap
    /// dispatched. `None` when the dispatch has no component source
    /// (broadcast / session / substrate-origin mail cannot open a stream) —
    /// so a handler stores an `Option<ResponseStream>` and sends only once
    /// the first grant has armed it.
    #[must_use]
    pub fn from_credit(ctx: &impl OutboundReply, credit: &HttpStreamCredit) -> Option<Self> {
        Some(Self { counterparty: ctx.source_mailbox()?, stream_id: credit.stream_id })
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
    /// The mailbox that opened the request stream — every credit grant on
    /// this handle targets it.
    pub counterparty: MailboxId,
    /// The stream id the cap assigned this upload (ADR-0128), stamped on
    /// every credit grant.
    pub stream_id: u64,
}

impl RequestStream {
    /// Capture the handle from the [`HttpRequestStreamOpen`] that begins a
    /// streamed upload. `None` when the dispatch has no component source
    /// (the same guard as [`ResponseStream::from_credit`]).
    #[must_use]
    pub fn from_open(ctx: &impl OutboundReply, open: &HttpRequestStreamOpen) -> Option<Self> {
        Some(Self { counterparty: ctx.source_mailbox()?, stream_id: open.stream_id })
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
    /// The mailbox that owns the upgraded connection — every outbound
    /// message and close on this handle targets it.
    pub counterparty: MailboxId,
    /// The connection's stream id (ADR-0132), stamped on every outbound
    /// message and close.
    pub stream_id: u64,
}

impl WebSocketStream {
    /// Capture the handle from the first [`HttpStreamCredit`] grant, which
    /// the cap sends at accept time before any peer traffic (ADR-0132).
    /// `None` when the dispatch has no component source.
    #[must_use]
    //noinspection DuplicatedCode -- response and websocket handles are distinct public protocol types.
    pub fn from_credit(ctx: &impl OutboundReply, credit: &HttpStreamCredit) -> Option<Self> {
        Some(Self { counterparty: ctx.source_mailbox()?, stream_id: credit.stream_id })
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

#[cfg(test)]
mod tests {
    use aether_actor::{HandlesKind, Singleton};
    use aether_data::{Kind, KindId};

    use super::super::kinds::HttpMethod;
    use super::*;

    /// A `MailSender` + `OutboundReply` that records every
    /// `send_detached_to` and reports a configurable inbound source, so the
    /// handle constructors and send methods are exercisable with no
    /// transport. Only `send_detached_to` and `source_mailbox` carry a real
    /// body — the handles use nothing else, so the rest assert their own
    /// disuse.
    #[derive(Default)]
    struct RecordingCtx {
        source: Option<MailboxId>,
        sent: Vec<(MailboxId, KindId, Vec<u8>)>,
    }

    impl MailSender for RecordingCtx {
        fn send<R, K>(&mut self, _payload: &K)
        where
            R: Singleton + HandlesKind<K>,
            K: Kind,
        {
            unreachable!("stream handles send only via send_detached_to")
        }

        fn send_many<R, K>(&mut self, _payloads: &[K])
        where
            R: Singleton + HandlesKind<K>,
            K: Kind + bytemuck::NoUninit,
        {
            unreachable!("stream handles send only via send_detached_to")
        }

        fn send_to_named<K: Kind>(&mut self, _name: &str, _payload: &K) {
            unreachable!("stream handles send only via send_detached_to")
        }

        fn prev_correlation(&self) -> u64 {
            0
        }

        fn send_detached_to<K: Kind>(&mut self, id: MailboxId, payload: &K) {
            self.sent.push((id, K::ID, payload.encode_into_bytes()));
        }
    }

    impl OutboundReply for RecordingCtx {
        type ReplyHandle = ();

        fn reply_target(&self) -> Option<()> {
            None
        }

        fn source_mailbox(&self) -> Option<MailboxId> {
            self.source
        }

        fn reply<K: Kind>(&mut self, _payload: &K) {
            unreachable!("stream handles never reply")
        }

        fn reply_to<K: Kind>(&mut self, _sender: (), _payload: &K) {
            unreachable!("stream handles never reply")
        }
    }

    const CAP: MailboxId = MailboxId(0x00C0_FFEE);

    /// The one send this handler recorded, decoded to `K`. Panics if it did
    /// not send exactly once, to the wrong recipient, or the wrong kind.
    fn only_send<K: Kind>(ctx: &RecordingCtx, recipient: MailboxId) -> K {
        assert_eq!(ctx.sent.len(), 1, "exactly one send");
        let (id, kind, bytes) = &ctx.sent[0];
        assert_eq!(*id, recipient, "recipient is the stored counterparty");
        assert_eq!(*kind, K::ID, "kind matches the send method");
        K::decode_from_bytes(bytes).expect("payload decodes back to its kind")
    }

    #[test]
    fn response_stream_captures_counterparty_and_stream_id() {
        let ctx = RecordingCtx { source: Some(CAP), ..RecordingCtx::default() };
        let handle = ResponseStream::from_credit(&ctx, &HttpStreamCredit { stream_id: 42, credit: 8 })
            .expect("a component-source credit opens the stream");
        assert_eq!(handle.counterparty, CAP);
        assert_eq!(handle.stream_id, 42);
    }

    #[test]
    fn from_credit_refuses_a_sourceless_dispatch() {
        let ctx = RecordingCtx::default();
        assert!(
            ResponseStream::from_credit(&ctx, &HttpStreamCredit { stream_id: 1, credit: 1 }).is_none(),
            "no component source → no handle"
        );
        assert!(WebSocketStream::from_credit(&ctx, &HttpStreamCredit { stream_id: 1, credit: 1 }).is_none());
    }

    #[test]
    fn response_stream_chunk_and_end_target_the_counterparty() {
        let handle = ResponseStream { counterparty: CAP, stream_id: 7 };

        let mut ctx = RecordingCtx::default();
        handle.chunk(&mut ctx, b"body-piece".to_vec());
        let chunk: HttpResponseChunk = only_send(&ctx, CAP);
        assert_eq!(chunk.stream_id, 7);
        assert_eq!(chunk.body, b"body-piece");

        let mut ctx = RecordingCtx::default();
        handle.end(&mut ctx);
        let end: HttpResponseStreamEnd = only_send(&ctx, CAP);
        assert_eq!(end.stream_id, 7);
    }

    #[test]
    fn request_stream_credit_targets_the_counterparty() {
        let ctx = RecordingCtx { source: Some(CAP), ..RecordingCtx::default() };
        let handle = RequestStream::from_open(
            &ctx,
            &HttpRequestStreamOpen {
                stream_id: 9,
                method: HttpMethod::Post,
                path: "/upload".to_string(),
                query: String::new(),
                headers: Vec::new(),
            },
        )
        .expect("a component-source open arms the request stream");

        let mut ctx = RecordingCtx::default();
        handle.credit(&mut ctx, 4);
        let credit: HttpRequestCredit = only_send(&ctx, CAP);
        assert_eq!(credit.stream_id, 9);
        assert_eq!(credit.credit, 4);
    }

    #[test]
    fn websocket_message_and_close_target_the_counterparty() {
        let handle = WebSocketStream { counterparty: CAP, stream_id: 3 };

        let mut ctx = RecordingCtx::default();
        handle.message(&mut ctx, true, b"\x00\x01".to_vec());
        let msg: WebSocketMessage = only_send(&ctx, CAP);
        assert_eq!(msg.stream_id, 3);
        assert!(msg.binary);
        assert_eq!(msg.data, b"\x00\x01");

        let mut ctx = RecordingCtx::default();
        handle.close(&mut ctx, 1000, "bye".to_string());
        let close: WebSocketClose = only_send(&ctx, CAP);
        assert_eq!(close.stream_id, 3);
        assert_eq!(close.code, 1000);
        assert_eq!(close.reason, "bye");
    }
}
