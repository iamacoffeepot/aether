//! The streaming fixtures (ADR-0128): two well-behaved response-streaming
//! handlers that pace chunks against credit, a flooder that ignores it, and
//! the request-side streaming upload handler.

use aether_actor::{Manual, actor};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::kinds::{
    HttpHeader, HttpRequestChunk, HttpRequestStreamEnd, HttpRequestStreamOpen, HttpResponseStreamOpen,
    HttpServerRequest, HttpServerResponse, HttpStreamCredit,
};
use crate::{RequestStream, ResponseStream};

use super::bind_catch_all;

/// The number of body chunks [`StreamHttpHandler`] emits. Chosen well
/// above the test's credit window so the round trip exercises credit
/// replenishment across many refills, not just the initial grant.
pub const STREAM_CHUNK_COUNT: u32 = 40;

/// The bytes of chunk `index`: its zero-padded index, so the reassembled
/// body is the deterministic concatenation `"000001…039"` a test can
/// rebuild and compare against.
pub fn stream_chunk_body(index: u32) -> Vec<u8> {
    format!("{index:03}").into_bytes()
}

/// A well-behaved response-streaming handler (ADR-0128): replies
/// `HttpResponseStreamOpen`, then emits [`STREAM_CHUNK_COUNT`] chunks
/// paced strictly against the credit it is granted, and terminates with
/// `HttpResponseStreamEnd`.
pub struct StreamHttpHandler;

/// Per-stream progress for [`StreamHttpHandler`].
pub struct StreamHttpHandlerState {
    next_index: u32,
    ended: bool,
}

#[actor(singleton, root)]
impl NativeActor for StreamHttpHandler {
    type State = StreamHttpHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_stream_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<StreamHttpHandlerState, BootError> {
        Ok(StreamHttpHandlerState { next_index: 0, ended: false })
    }

    /// The cap reads this handler's accept-set off the catch-all
    /// binding to take the streaming path.
    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::single]
    fn on_request(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _request: HttpServerRequest,
    ) -> HttpResponseStreamOpen {
        state.next_index = 0;
        state.ended = false;
        HttpResponseStreamOpen {
            status: 200,
            headers: vec![HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() }],
        }
    }

    /// Spend the granted credit: send up to `credit.credit` more chunks,
    /// then terminate once all [`STREAM_CHUNK_COUNT`] have gone out.
    /// Addressed through the ADR-0133 [`ResponseStream`] handle — the
    /// data phase goes to whichever dispatch shard granted the credit,
    /// never to the supervisor by type (ADR-0135).
    #[handler::manual]
    fn on_credit(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, credit: HttpStreamCredit) {
        let Some(stream) = ResponseStream::from_credit(ctx, &credit) else {
            return;
        };
        let mut budget = credit.credit;
        while budget > 0 && state.next_index < STREAM_CHUNK_COUNT {
            stream.chunk(ctx, stream_chunk_body(state.next_index));
            state.next_index += 1;
            budget -= 1;
        }
        if state.next_index >= STREAM_CHUNK_COUNT && !state.ended {
            stream.end(ctx);
            state.ended = true;
        }
    }
}

/// A response-streaming handler whose entire body is the `stream_id` the
/// cap minted for that stream and handed over in the first
/// `HttpStreamCredit`. It exists so a test can read the id off the wire
/// without reaching into handler state.
pub struct StreamIdEchoHandler;

/// Guards [`StreamIdEchoHandler`] against re-emitting on replenishment
/// credit; reset per request, so each request emits exactly one body.
pub struct StreamIdEchoHandlerState {
    emitted: bool,
}

#[actor(singleton, root)]
impl NativeActor for StreamIdEchoHandler {
    type State = StreamIdEchoHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_stream_id_echo_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<StreamIdEchoHandlerState, BootError> {
        Ok(StreamIdEchoHandlerState { emitted: false })
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::single]
    fn on_request(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _request: HttpServerRequest,
    ) -> HttpResponseStreamOpen {
        state.emitted = false;
        HttpResponseStreamOpen {
            status: 200,
            headers: vec![HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() }],
        }
    }

    #[handler::manual]
    fn on_credit(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, credit: HttpStreamCredit) {
        let Some(stream) = ResponseStream::from_credit(ctx, &credit) else {
            return;
        };
        if state.emitted {
            return;
        }
        stream.chunk(ctx, credit.stream_id.to_string().into_bytes());
        stream.end(ctx);
        state.emitted = true;
    }
}

/// The number of chunks [`FloodHttpHandler`] blasts on its first credit,
/// far more than any small test window — enough that the cap's credit
/// accounting hits zero and the over-window guard tears the stream down.
pub const FLOOD_CHUNK_COUNT: u32 = 200;

/// A misbehaving response-streaming handler (ADR-0128 trust boundary):
/// it replies `HttpResponseStreamOpen`, then on its first credit ignores
/// the granted amount entirely and floods [`FLOOD_CHUNK_COUNT`] chunks.
pub struct FloodHttpHandler;

/// Guards [`FloodHttpHandler`] against re-flooding on replenishment
/// credit (which never arrives once the cap tears the stream down).
pub struct FloodHttpHandlerState {
    flooded: bool,
}

#[actor(singleton, root)]
impl NativeActor for FloodHttpHandler {
    type State = FloodHttpHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_flood_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<FloodHttpHandlerState, BootError> {
        Ok(FloodHttpHandlerState { flooded: false })
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::single]
    fn on_request(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _request: HttpServerRequest,
    ) -> HttpResponseStreamOpen {
        HttpResponseStreamOpen { status: 200, headers: Vec::new() }
    }

    #[handler::manual]
    fn on_credit(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, credit: HttpStreamCredit) {
        if state.flooded {
            return;
        }
        state.flooded = true;
        let Some(stream) = ResponseStream::from_credit(ctx, &credit) else {
            return;
        };
        for _ in 0..FLOOD_CHUNK_COUNT {
            stream.chunk(ctx, vec![b'x'; 8]);
        }
    }
}

/// A streaming *upload* handler (ADR-0128), the request-side mirror of
/// [`StreamHttpHandler`]: it declares the request-stream vocabulary
/// (`HttpRequestStreamOpen` in its accept-set is the structural opt-in the
/// cap reads), grants one credit per [`HttpRequestChunk`] it drains,
/// accumulates the received byte count, and replies `200` echoing that
/// count when the stream ends — the reply riding the
/// [`HttpRequestStreamEnd`] correlation.
pub struct StreamingUploadHandler;

/// Per-upload progress for [`StreamingUploadHandler`].
pub struct StreamingUploadHandlerState {
    received: usize,
    /// The ADR-0133 inbound-stream handle captured at
    /// `HttpRequestStreamOpen` — credit grants go to whichever dispatch
    /// shard opened the stream (ADR-0135), never to the supervisor by
    /// type.
    stream: Option<RequestStream>,
}

#[actor(singleton, root)]
impl NativeActor for StreamingUploadHandler {
    type State = StreamingUploadHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_upload_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<StreamingUploadHandlerState, BootError> {
        Ok(StreamingUploadHandlerState { received: 0, stream: None })
    }

    /// The cap reads this handler's accept-set off the catch-all
    /// binding to take the request-streaming path.
    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::manual]
    fn on_stream_open(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, open: HttpRequestStreamOpen) {
        state.received = 0;
        state.stream = RequestStream::from_open(ctx, &open);
    }

    /// Count the piece and grant one credit back so the cap delivers the
    /// next — the inbound mirror of [`StreamHttpHandler::on_credit`].
    #[handler::single]
    fn on_chunk(state: &mut Self::State, ctx: &mut NativeCtx<'_>, chunk: HttpRequestChunk) {
        state.received += chunk.body.len();
        if let Some(stream) = &state.stream {
            stream.credit(ctx, 1);
        }
    }

    #[handler::single]
    fn on_stream_end(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _end: HttpRequestStreamEnd,
    ) -> HttpServerResponse {
        HttpServerResponse {
            status: 200,
            headers: Vec::new(),
            body: format!("received:{}", state.received).into_bytes(),
        }
    }
}
