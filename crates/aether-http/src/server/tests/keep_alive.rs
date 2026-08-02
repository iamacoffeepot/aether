//! Connection lifetime: sequential (and pipelined) requests on one kept-alive
//! socket, socket reuse after a streamed response, the explicit
//! `Connection: close` and HTTP/1.0 close defaults, and the idle keep-alive
//! timeout.

use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::handlers::{EchoHttpHandler, STREAM_CHUNK_COUNT, StreamHttpHandler, stream_chunk_body};
use super::support::{
    assert_closed, body_of, boot_buffered, boot_chassis, boot_response_stream, dechunk, keep_alive_config_for, port_of,
    read_one_chunked_response, read_one_response, round_trip_live,
};

/// Two requests round-trip in order on one kept-alive socket (HTTP/1.1
/// default, no `Connection: close`), each response carrying `Connection:
/// keep-alive`; a final `Connection: close` request then terminates the
/// connection. The two requests are written pipelined (both before the first
/// response is read), so this also exercises the reader carrying request 2's
/// over-read bytes across the resume signal.
#[test]
fn keep_alive_serves_sequential_requests_on_one_socket() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the pipelined round trip.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    let mut carry: Vec<u8> = Vec::new();

    // Pipeline both requests, then read both responses in order.
    stream
        .write_all(
            b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\n\
              GET /two HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .expect("write pipelined requests");
    stream.flush().expect("flush");

    let first = read_one_response(&mut stream, &mut carry);
    assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "first response 200: {first:?}");
    assert!(first.contains("x-aether-path: /one\r\n"), "first response is /one: {first:?}");
    assert!(first.contains("Connection: keep-alive\r\n"), "first response keeps alive: {first:?}");

    let second = read_one_response(&mut stream, &mut carry);
    assert!(second.contains("x-aether-path: /two\r\n"), "second response is /two, in order: {second:?}");
    assert!(second.contains("Connection: keep-alive\r\n"), "second response keeps alive: {second:?}");

    // A final `Connection: close` request terminates the connection.
    stream
        .write_all(b"GET /three HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write closing request");
    stream.flush().expect("flush");
    let third = read_one_response(&mut stream, &mut carry);
    assert!(third.contains("x-aether-path: /three\r\n"), "third response is /three: {third:?}");
    assert!(third.contains("Connection: close\r\n"), "third response closes: {third:?}");
    assert_closed(&mut stream);
}

/// Tripwire (issue #2582): a streamed (chunked) response to a keep-alive
/// request renders `Connection: keep-alive` and the socket is reused for a
/// second request — the streamed mirror of
/// [`keep_alive_serves_sequential_requests_on_one_socket`]. Before the fix,
/// `render_stream_head` hardcoded `Connection: close` and `finish_stream`
/// unconditionally closed the connection, so this second read would hang /
/// fail against the pre-fix behavior.
#[test]
fn keep_alive_reuses_socket_after_streamed_response() {
    let chassis = boot_response_stream::<StreamHttpHandler>(8);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the persistent-socket reads.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    let mut carry: Vec<u8> = Vec::new();

    let expected: Vec<u8> = (0..STREAM_CHUNK_COUNT).flat_map(stream_chunk_body).collect();

    // First streamed request, HTTP/1.1 default (no `Connection: close`).
    stream.write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write first request");
    stream.flush().expect("flush");
    let first = read_one_chunked_response(&mut stream, &mut carry);
    assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "{first:?}");
    assert!(first.contains("Connection: keep-alive\r\n"), "streamed response keeps alive: {first:?}");
    assert_eq!(dechunk(body_of(&first)).into_bytes(), expected, "first stream reassembles in order");

    // The reuse invariant: a second request on the same socket after the
    // stream ended gets served rather than the socket being closed.
    stream.write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write second request");
    stream.flush().expect("flush");
    let second = read_one_chunked_response(&mut stream, &mut carry);
    assert!(second.starts_with("HTTP/1.1 200 OK\r\n"), "{second:?}");
    assert!(second.contains("Connection: keep-alive\r\n"), "second streamed response keeps alive too: {second:?}");
    assert_eq!(dechunk(body_of(&second)).into_bytes(), expected, "second stream reassembles in order");
}

/// Pins the negative alongside the reuse tripwire above: a streamed request
/// carrying `Connection: close` still tears the socket down once the stream
/// ends, exactly like the buffered path.
#[test]
fn streamed_response_honors_explicit_connection_close() {
    let chassis = boot_response_stream::<StreamHttpHandler>(8);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the explicit-close read.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    let mut carry: Vec<u8> = Vec::new();

    stream.write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").expect("write request");
    stream.flush().expect("flush");
    let response = read_one_chunked_response(&mut stream, &mut carry);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert!(response.contains("Connection: close\r\n"), "streamed response honors explicit close: {response:?}");
    assert_closed(&mut stream);
}

/// An HTTP/1.0 request with no `Connection` header closes by default: the
/// response carries `Connection: close` and the server closes the socket.
#[test]
fn http_1_0_defaults_to_close() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the HTTP/1.0 read.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    stream.write_all(b"GET /ten HTTP/1.0\r\nHost: localhost\r\n\r\n").expect("write request");
    stream.flush().expect("flush");

    let mut carry: Vec<u8> = Vec::new();
    let response = read_one_response(&mut stream, &mut carry);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200, got: {response:?}");
    assert!(response.contains("Connection: close\r\n"), "HTTP/1.0 defaults to close: {response:?}");
    assert_closed(&mut stream);
}

/// A kept-alive connection left idle between requests is closed by the
/// server after the configured `keep_alive_timeout_millis`, rather than
/// pinning the reader thread for the full request timeout.
#[test]
fn idle_kept_alive_connection_closes_after_timeout() {
    let chassis = boot_chassis::<EchoHttpHandler>(keep_alive_config_for(300));
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the kept-alive read.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    stream.write_all(b"GET /keep HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write request");
    stream.flush().expect("flush");

    let mut carry: Vec<u8> = Vec::new();
    let response = read_one_response(&mut stream, &mut carry);
    assert!(response.contains("Connection: keep-alive\r\n"), "kept-alive response: {response:?}");

    // Now idle. The 300 ms idle timeout closes the connection well before the
    // 5 s request timeout / read timeout would — the elapsed bound is the
    // tripwire distinguishing the idle close from a slow read timeout.
    let started = Instant::now();
    assert_closed(&mut stream);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "idle connection closed via the keep-alive timeout, not the request timeout",
    );
}
