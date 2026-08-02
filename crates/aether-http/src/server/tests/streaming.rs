//! Streaming in both directions (ADR-0128): a response streamed across many
//! credit refills, the over-window teardown of a flooding producer, distinct
//! per-stream ids, uploads streamed past the buffered body cap, and the
//! framing guards that stay in force on the streaming path.

use super::handlers::{
    EchoHttpHandler, FloodHttpHandler, STREAM_CHUNK_COUNT, StreamHttpHandler, StreamIdEchoHandler,
    StreamingUploadHandler, stream_chunk_body,
};
use super::support::{
    body_of, boot_request_stream, boot_response_stream, dechunk, port_of, round_trip, round_trip_live,
};

/// A streaming handler (ADR-0128) emits its body across more chunks than the
/// credit window, and the cap streams them as chunked transfer-encoding that
/// the client reassembles intact — exercising credit replenishment across
/// many refills, not just the initial grant.
#[test]
fn streamed_response_reassembles_across_credit_window() {
    // Window well below the chunk count so credit must replenish.
    let chassis = boot_response_stream::<StreamHttpHandler>(8);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port_of(&chassis), b"GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert!(response.contains("Transfer-Encoding: chunked\r\n"), "streamed response is chunked: {response:?}");
    assert!(!response.contains("Content-Length:"), "streamed response omits Content-Length: {response:?}");

    let expected: Vec<u8> = (0..STREAM_CHUNK_COUNT).flat_map(stream_chunk_body).collect();
    assert_eq!(
        dechunk(body_of(&response)).into_bytes(),
        expected,
        "reassembled body matches every emitted chunk in order",
    );
}

/// Tripwire: a handler that floods chunks past its granted credit
/// (ADR-0128 §Consequences trust boundary) is torn down by the cap — the
/// response head and some chunks arrive, but the stream never reaches its
/// terminating zero-length chunk, so a misbehaving producer cannot outrun
/// the window unbounded.
#[test]
fn over_window_flood_tears_the_stream_down() {
    // Tiny window so the flood overruns credit within a few chunks.
    let chassis = boot_response_stream::<FloodHttpHandler>(2);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port_of(&chassis), b"GET /flood HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert!(
        response.contains("Transfer-Encoding: chunked\r\n"),
        "flood stream head is chunked before teardown: {response:?}",
    );
    assert!(!response.contains("0\r\n\r\n"), "flood stream is torn down before the terminator: {response:?}");
}

/// A multi-megabyte `Content-Length` upload — past the 1 `MiB` buffered
/// `max_request_bytes` cap — streams incrementally to a streaming handler and
/// the echoed byte count matches, so the body never resides whole in the reader
/// or the handler and the buffered cap does not `413` it (ADR-0128). The small
/// window forces credit to replenish across hundreds of chunks.
#[test]
fn large_upload_streams_past_the_buffered_cap() {
    let chassis = boot_request_stream::<StreamingUploadHandler>(4);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live with a cheap zero-length chunked
    // upload before the multi-megabyte assertion, so the large body is not
    // re-sent on each poll iteration.
    round_trip_live(port, b"POST /warmup HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n");

    // Well past DEFAULT_MAX_REQUEST_BYTES (1 MiB) — a buffered handler would
    // `413` this; the streaming handler takes it incrementally.
    let body_len = 2 * 1024 * 1024 + 7;
    let mut request =
        format!("POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: {body_len}\r\n\r\n").into_bytes();
    request.resize(request.len() + body_len, b'a');

    let response = round_trip(port, &request);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert_eq!(body_of(&response), format!("received:{body_len}"), "the streamed byte count round-trips");
}

/// A `Transfer-Encoding: chunked` upload to a streaming handler is accepted and
/// decoded incrementally (not `411`), the hand-rolled chunked decoder
/// reassembling the body across frames (ADR-0128).
#[test]
fn chunked_upload_streams_to_streaming_handler() {
    let chassis = boot_request_stream::<StreamingUploadHandler>(4);
    let port = port_of(&chassis);

    // "hello" (5) + " world" (6) = 11 body bytes across two chunks.
    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(
        port,
        b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n\
          5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "chunked upload accepted, not 411: {response:?}");
    assert_eq!(body_of(&response), "received:11", "the two chunks decoded to 11 body bytes");
}

/// A request carrying both `Content-Length` and `Transfer-Encoding` (the
/// request-smuggling shape) is refused `411` even for a streaming handler —
/// ADR-0128 relaxes the guard only for a *lone* `chunked` coding, never the
/// ambiguous pair.
#[test]
fn content_length_with_transfer_encoding_is_411() {
    let chassis = boot_request_stream::<StreamingUploadHandler>(4);
    let port = port_of(&chassis);

    let response = round_trip(
        port,
        b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello",
    );
    assert!(
        response.starts_with("HTTP/1.1 411 "),
        "smuggling shape stays 411 even for a streaming handler: {response:?}",
    );
}

/// A websocket-upgrade request carrying a lone `Transfer-Encoding: chunked`
/// body is answered `411`, even against a *streaming* handler that would
/// otherwise take a lone chunked body on the non-upgrade path
/// (`chunked_upload_streams_to_streaming_handler`) — the upgrade forces
/// buffering (RFC 6455: a handshake carries no body), so there is nothing
/// to buffer under, the sharpest form of the regression.
///
/// Tripwire: on `origin/main` this returns non-411 because the framing
/// reject sits inside `if ws_key.is_none()` and never runs for a valid
/// upgrade handshake.
#[test]
fn chunked_on_ws_upgrade_is_411() {
    let chassis = boot_request_stream::<StreamingUploadHandler>(4);
    let port = port_of(&chassis);

    let response = round_trip(
        port,
        b"GET /ws HTTP/1.1\r\n\
          Host: localhost\r\n\
          Upgrade: websocket\r\n\
          Connection: Upgrade\r\n\
          Sec-WebSocket-Version: 13\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 411 "), "expected 411, got: {response:?}");
}

/// A buffered handler (no `HttpRequestStreamOpen` in its accept-set) keeps the
/// unchanged `HttpServerRequest` round trip — the streaming decision is a
/// per-handler property, so an ordinary `Content-Length` POST to
/// [`EchoHttpHandler`] still buffers and echoes verbatim (ADR-0128). The
/// contrast case (`transfer_encoding_is_411`) shows the same handler cannot
/// take a chunked body.
#[test]
fn buffered_handler_keeps_the_unstreamed_path() {
    let chassis = boot_request_stream::<EchoHttpHandler>(4);
    let port = port_of(&chassis);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port, b"POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert_eq!(body_of(&response), "hello", "buffered body echoed verbatim");
}

/// Tripwire: two consecutive response streams must carry distinct
/// `stream_id`s. The id is the key of the cap's stream table and of the
/// credit accounting that guards it, so a repeated id lets one stream's
/// credit grants and teardown act on another's state — with the id constant,
/// a stale grant is byte-indistinguishable from a fresh one and drives a
/// correct handler past its window into the over-window teardown (issue
/// 3730). Response streams once reused the dispatch correlation id, which is
/// minted per sender and so repeated across requests; ADR-0128 §2 was amended
/// on 2026-07-20 to mint them from the cap's monotonic counter instead. The
/// handler echoes its own granted `stream_id` as the body, so this reads the
/// two ids off the wire and only asserts they differ — never their values,
/// which would pin the counter's start point.
#[test]
fn consecutive_response_streams_get_distinct_stream_ids() {
    let chassis = boot_response_stream::<StreamIdEchoHandler>(8);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the measured requests.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let first = round_trip(port, b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let second = round_trip(port, b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let first_id = dechunk(body_of(&first));
    let second_id = dechunk(body_of(&second));

    assert!(!first_id.is_empty(), "first stream reported no id: {first:?}");
    assert!(!second_id.is_empty(), "second stream reported no id: {second:?}");
    assert_ne!(first_id, second_id, "consecutive response streams reused stream_id {first_id}");
}
