//! Reference HTTP handler fixture for the `serving-http` e2e test and
//! recipe (issue 1762, ADR-0108). Not a demo, not exemplary — its only
//! job is to prove the `aether.http.server` guest load path end to end:
//! `HttpServerCapability` dispatches an `HttpServerRequest` here; this
//! actor path-matches and replies `HttpServerResponse`; the cap formats
//! the HTTP/1.1 response and writes it to the client socket.
//!
//! Behaviour:
//!
//! - `GET /` → 200 `hello from aether`
//! - Anything else → 404 `not found`
//!
//! Registered at `aether.component/aether.embedded:web` after load.
//! The e2e test configures `HttpServerConfig.handler_mailbox` to that
//! address and then fires real `TcpStream` requests at the bound port.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references. A stateless handler that
// ignores `self` is correct but triggers `unused_self`.
#![allow(clippy::needless_pass_by_value, clippy::unused_self)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::http::HttpServerCapability;
use aether_capabilities::http::kinds::{
    HttpResponseChunk, HttpResponseStreamEnd, HttpResponseStreamOpen, HttpServerRequest,
    HttpServerResponse, HttpStreamCredit,
};

pub struct HttpHandler;

#[actor]
impl WasmActor for HttpHandler {
    const NAMESPACE: &'static str = "web";

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
    /// to `"aether.component/aether.embedded:web"` to route here.
    #[handler]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, req: HttpServerRequest) -> HttpServerResponse {
        let (status, body): (u16, &[u8]) = match req.path.as_str() {
            "/" => (200, b"hello from aether"),
            _ => (404, b"not found"),
        };
        HttpServerResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
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
/// Registered at `aether.component/aether.embedded:web_stream` after load.
pub struct StreamingHttpHandler {
    /// The stream this handler is feeding, learned from the first credit mail.
    stream_id: u64,
    /// Index of the next chunk to emit.
    next_index: u32,
    /// Whether the terminator has been sent.
    ended: bool,
}

#[actor]
impl WasmActor for StreamingHttpHandler {
    const NAMESPACE: &'static str = "web_stream";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(StreamingHttpHandler {
            stream_id: 0,
            next_index: 0,
            ended: false,
        })
    }

    /// Open a streamed `200` response. The body arrives later, one chunk per
    /// unit of credit the cap grants.
    ///
    /// # Agent
    /// Not sent manually — the `aether.http.server` cap dispatches it on
    /// every inbound request. Route here by pointing
    /// `HttpServerConfig.handler_mailbox` at
    /// `"aether.component/aether.embedded:web_stream"`.
    #[handler]
    fn on_request(
        &mut self,
        _ctx: &mut WasmCtx<'_>,
        _req: HttpServerRequest,
    ) -> HttpResponseStreamOpen {
        self.next_index = 0;
        self.ended = false;
        HttpResponseStreamOpen {
            status: 200,
            headers: Vec::new(),
        }
    }

    /// Spend the granted credit: emit up to `credit.credit` more chunks, then
    /// terminate once all `STREAM_CHUNK_COUNT` have gone out.
    ///
    /// # Agent
    /// Not sent manually — the cap sends one `HttpStreamCredit` per freed
    /// window slot; the handler emits at most that many `HttpResponseChunk`s
    /// in response.
    #[handler]
    fn on_credit(&mut self, ctx: &mut WasmCtx<'_>, credit: HttpStreamCredit) {
        self.stream_id = credit.stream_id;
        let mut budget = credit.credit;
        while budget > 0 && self.next_index < STREAM_CHUNK_COUNT {
            ctx.actor::<HttpServerCapability>()
                .send(&HttpResponseChunk {
                    stream_id: self.stream_id,
                    body: format!("chunk-{}\n", self.next_index).into_bytes(),
                });
            self.next_index += 1;
            budget -= 1;
        }
        if self.next_index >= STREAM_CHUNK_COUNT && !self.ended {
            ctx.actor::<HttpServerCapability>()
                .send(&HttpResponseStreamEnd {
                    stream_id: self.stream_id,
                });
            self.ended = true;
        }
    }
}
