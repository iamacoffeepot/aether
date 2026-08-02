//! Fixtures shared by every server test module: the chassis boot helpers, the
//! raw-socket client round trips, and the response readers.

use aether_substrate::actor::native::NativeActor;
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::testing::{TestChassis, fresh_substrate};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::server::{HttpServerCapability, HttpServerConfig, HttpServerHandle};

use super::handlers::FixedBodyHttpHandler;

pub(super) fn config_for(max_request_bytes: usize) -> HttpServerConfig {
    HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        max_request_bytes,
        request_timeout_millis: 5_000,
        ..HttpServerConfig::default()
    }
}

/// Server config for the streaming tests (ADR-0128): a small credit window
/// so a multi-chunk response must replenish credit repeatedly, and a flood
/// overruns it fast.
fn stream_config_for(window: u32) -> HttpServerConfig {
    HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        response_stream_window: window,
        ..HttpServerConfig::default()
    }
}

/// Server config for the request-streaming tests (ADR-0128): a small inbound
/// credit window so a multi-chunk upload replenishes credit repeatedly. The
/// `max_request_bytes` cap stays the 1 `MiB` default, which the large-upload
/// test deliberately exceeds — streaming bypasses the buffered body cap.
fn request_stream_config_for(window: u32) -> HttpServerConfig {
    HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        request_stream_window: window,
        ..HttpServerConfig::default()
    }
}

/// Server config with a short idle (keep-alive) timeout, for the
/// idle-close test — every other field matches [`config_for`].
pub(super) fn keep_alive_config_for(keep_alive_timeout_millis: u64) -> HttpServerConfig {
    HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        keep_alive_timeout_millis,
        ..HttpServerConfig::default()
    }
}

/// Boot a passive chassis holding one handler actor `H` plus the HTTP
/// server cap under `config` — the single-handler shape most server
/// tests share. Multi-handler and cap-only boots stay explicit at their
/// call sites.
pub(super) fn boot_chassis<H>(config: HttpServerConfig) -> PassiveChassis<TestChassis>
where
    H: aether_actor::Root + NativeActor<Config = (), Params = ()>,
{
    let (registry, mailer) = fresh_substrate();
    Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<H>(())
        .with_actor_configured::<HttpServerCapability>((), config)
        .build_passive()
        .expect("caps boot")
}

pub(super) fn boot_single_shard_fixed_body() -> PassiveChassis<TestChassis> {
    boot_chassis::<FixedBodyHttpHandler>(HttpServerConfig {
        enabled: true,
        bind_addr: "127.0.0.1:0".to_string(),
        request_timeout_millis: 5_000,
        dispatch_shards: 1,
        ..HttpServerConfig::default()
    })
}

/// [`boot_chassis`] with `H` (its `wire` binds the `/` catch-all) under a
/// buffered [`config_for`] config (`max_request_bytes`).
pub(super) fn boot_buffered<H>(max_request_bytes: usize) -> PassiveChassis<TestChassis>
where
    H: aether_actor::Root + NativeActor<Config = (), Params = ()>,
{
    boot_chassis::<H>(config_for(max_request_bytes))
}

/// [`boot_chassis`] with `H` under a response-streaming [`stream_config_for`]
/// config (credit `window`).
pub(super) fn boot_response_stream<H>(window: u32) -> PassiveChassis<TestChassis>
where
    H: aether_actor::Root + NativeActor<Config = (), Params = ()>,
{
    boot_chassis::<H>(stream_config_for(window))
}

/// [`boot_chassis`] with `H` under a request-streaming
/// [`request_stream_config_for`] config (credit `window`).
pub(super) fn boot_request_stream<H>(window: u32) -> PassiveChassis<TestChassis>
where
    H: aether_actor::Root + NativeActor<Config = (), Params = ()>,
{
    boot_chassis::<H>(request_stream_config_for(window))
}

pub(super) fn port_of(chassis: &PassiveChassis<TestChassis>) -> u16 {
    chassis.handle::<HttpServerHandle>().expect("HttpServerHandle published").local_port
}

/// Insert `Connection: close` as the last header of a complete request's
/// head. Keep-alive is the HTTP/1.1 default, so a single-shot round-trip
/// that reads to EOF must opt the connection into close; injecting it here
/// keeps every single-shot test's request literal focused on what it
/// exercises. The keep-alive / HTTP-1.0 / idle-timeout tests drive their own
/// sockets and do not go through this helper.
fn with_connection_close(request: &[u8]) -> Vec<u8> {
    let terminator = b"\r\n\r\n";
    let Some(pos) = request.windows(terminator.len()).position(|window| window == terminator) else {
        return request.to_vec();
    };
    let mut out = Vec::with_capacity(request.len() + 19);
    out.extend_from_slice(&request[..pos]);
    out.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    out.extend_from_slice(&request[pos + terminator.len()..]);
    out
}

/// Open a client `TcpStream` to the server's OS-picked port, write the raw
/// request (with `Connection: close` appended, see [`with_connection_close`]),
/// and read the full response (the cap closes after the single response, so
/// the read terminates at EOF).
pub(super) fn round_trip(port: u16, request: &[u8]) -> String {
    let request = with_connection_close(request);
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    stream.write_all(&request).expect("write request");
    stream.flush().expect("flush request");

    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&response).into_owned()
}

/// Round-trip `request`, retrying past the pre-registration `503` until the
/// catch-all route's async `wire` mail (ADR-0130) has landed, then return
/// that first non-`503` response. The head/header-asserting sibling of
/// [`poll_body`] (which only matches on the body) for a catch-all route's
/// first positive assertion. None of these handlers legitimately reply
/// `503`, so the first non-`503` is the live response.
pub(super) fn round_trip_live(port: u16, request: &[u8]) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = round_trip(port, request);
        if !response.starts_with("HTTP/1.1 503 ") {
            return response;
        }
        assert!(Instant::now() < deadline, "route did not go live within 10s; last response: {response:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

/// Poll `request` until its response body equals `expected` (bounded
/// deadline, house pattern — see `tcp/tests.rs` / the bundle
/// `http_serving.rs`). The `wire` route registrations are asynchronous
/// mail, so a route's *first* positive assertion must poll it live
/// rather than race the registration; assertions that depend on an
/// already-proven-live route can then be direct.
pub(super) fn poll_body(port: u16, request: &[u8], expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = round_trip(port, request);
        if body_of(&response) == expected {
            return;
        }
        assert!(Instant::now() < deadline, "expected body {expected:?} within 10s; last response: {response:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

pub(super) fn body_of(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// Index of the first `\r\n` at or after `from`, or `None`.
fn find_crlf(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len().saturating_sub(1)).find(|&i| bytes[i] == b'\r' && bytes[i + 1] == b'\n')
}

/// Reassemble a chunked transfer-encoding body (everything after the head's
/// blank line) into its payload, stopping at the zero-length terminator.
pub(super) fn dechunk(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut pos = 0;
    let mut out: Vec<u8> = Vec::new();
    while pos < bytes.len() {
        let Some(crlf) = find_crlf(bytes, pos) else {
            break;
        };
        let size = usize::from_str_radix(body[pos..crlf].trim(), 16).unwrap_or(0);
        pos = crlf + 2;
        if size == 0 {
            break;
        }
        if pos + size > bytes.len() {
            break;
        }
        out.extend_from_slice(&bytes[pos..pos + size]);
        // Advance past the chunk body and its trailing CRLF.
        pos += size + 2;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read from `stream` into `carry` until the blank line terminating the
/// HTTP response head is buffered; return the byte index just past it.
/// Shared by the buffered and chunked response readers.
fn read_response_head(stream: &mut TcpStream, carry: &mut Vec<u8>, chunk: &mut [u8]) -> usize {
    loop {
        if let Some(pos) = carry.windows(4).position(|window| window == b"\r\n\r\n") {
            return pos + 4;
        }
        let n = stream.read(chunk).expect("read response head");
        assert!(n > 0, "eof before response head; buffered: {:?}", String::from_utf8_lossy(carry));
        carry.extend_from_slice(&chunk[..n]);
    }
}

/// Read exactly one HTTP/1.1 response off `stream` — the head up to its
/// blank line, then its `Content-Length` body — leaving any bytes read past
/// it (a pipelined next response) in `carry` for the following call. Panics
/// on EOF mid-response, so a test that expects the connection to close reads
/// one response and then asserts EOF separately.
pub(super) fn read_one_response(stream: &mut TcpStream, carry: &mut Vec<u8>) -> String {
    let mut chunk = [0u8; 4096];
    let head_end = read_response_head(stream, carry, &mut chunk);
    let content_length = content_length_of(&carry[..head_end]);
    while carry.len() < head_end + content_length {
        let n = stream.read(&mut chunk).expect("read response body");
        assert!(n > 0, "eof mid response body");
        carry.extend_from_slice(&chunk[..n]);
    }
    let response = String::from_utf8_lossy(&carry[..head_end + content_length]).into_owned();
    carry.drain(..head_end + content_length);
    response
}

/// Parse the `Content-Length` from a response head (case-insensitive), `0`
/// when absent.
fn content_length_of(head: &[u8]) -> usize {
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// Read exactly one *chunked* transfer-encoding HTTP/1.1 response off
/// `stream` (a streamed response, ADR-0128) — the head up to its blank line,
/// then the chunked body up to and including its terminating zero-length
/// chunk (`0\r\n\r\n`) — leaving any bytes read past it (a pipelined next
/// response) in `carry` for the following call. The chunked mirror of
/// [`read_one_response`], which only handles the buffered `Content-Length`
/// case.
pub(super) fn read_one_chunked_response(stream: &mut TcpStream, carry: &mut Vec<u8>) -> String {
    let mut chunk = [0u8; 4096];
    let head_end = read_response_head(stream, carry, &mut chunk);
    let terminator = b"0\r\n\r\n";
    let body_end = loop {
        if let Some(pos) = carry[head_end..].windows(terminator.len()).position(|window| window == terminator) {
            break head_end + pos + terminator.len();
        }
        let n = stream.read(&mut chunk).expect("read chunked response body");
        assert!(n > 0, "eof mid chunked response body");
        carry.extend_from_slice(&chunk[..n]);
    };
    let response = String::from_utf8_lossy(&carry[..body_end]).into_owned();
    carry.drain(..body_end);
    response
}

/// Assert that the next read on `stream` observes the server's close (EOF).
/// The stream's read timeout bounds this so a server that failed to close
/// surfaces as a timeout rather than a hang.
pub(super) fn assert_closed(stream: &mut TcpStream) {
    let mut tail = [0u8; 64];
    let read = stream.read(&mut tail);
    assert!(matches!(read, Ok(0)), "expected the server to close the connection, got: {read:?}");
}
