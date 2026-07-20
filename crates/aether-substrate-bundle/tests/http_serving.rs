//! End-to-end smoke for the `aether.http.server` guest handler path
//! (issue 1762, ADR-0108). Loads the `http_handler` fixture component
//! into a headless chassis with `HttpServerCapability` bound, fires a
//! real HTTP/1.1 request over a `TcpStream`, and asserts the returned
//! status line and body. This proves the full stack:
//!
//! ```text
//! TcpStream → HttpServerCapability → aether.component.load (wasm guest)
//!           → HttpServerRequest dispatch → WasmActor::on_request
//!           → HttpServerResponse reply → formatted HTTP/1.1 response
//! ```
//!
//! Heavy: boots a full headless chassis with a real wasm guest, so it
//! lives in `mod tests::heavy` and runs in the `serial-heavy` nextest
//! group.  Skipped when `http_handler.wasm` hasn't been pre-built
//! (`AETHER_REQUIRE_RUNTIME=1` flips the skip to a panic, same as the
//! other wasm-gated integration tests).

// Skip diagnostic goes to stderr so `cargo nextest` surfaces it
// alongside `test ... ok`.
#![allow(clippy::print_stderr)]
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::str::from_utf8;
use std::thread;
use std::time::{Duration, Instant};

use aether_anthropic::AnthropicConfig;
use aether_component::WasmTrampoline;
use aether_contentgen::ContentGenConfig;
use aether_gemini::GeminiConfig;
use aether_http::HttpConfig;
use aether_http::{HttpServerConfig, HttpServerHandle};
use aether_substrate_bundle::Chassis as _;
use aether_substrate_bundle::autoload::AutoloadComponent;
use aether_substrate_bundle::headless::{HeadlessChassis, HeadlessEnv};
use aether_substrate_bundle::test_bench::test_helpers::{
    init_save_sandbox, locate_component_wasm, test_namespace_roots,
};

/// The `http_handler` fixture's `NAMESPACE` const — the subname under
/// which `WasmTrampoline` registers it, and the last segment of its
/// full lineage address (`aether.component/aether.embedded:test.web`).
/// The handler binds the `/` catch-all route in its `wire` hook.
const HANDLER_NAMESPACE: &str = "test.web";

/// The `StreamingHttpHandler` fixture's `NAMESPACE` const (ADR-0128).
const STREAM_HANDLER_NAMESPACE: &str = "test.web_stream";

/// The chunk count `StreamingHttpHandler` emits — kept in step with the
/// fixture's own `STREAM_CHUNK_COUNT`.
const STREAM_CHUNK_COUNT: u32 = 20;

/// The `RoutedStreamingHttpHandler` fixture's `NAMESPACE` const — a
/// response-streaming handler reached through an externally-registered
/// route (`register_route_self { prefix: "/routed-stream" }` in `wire`)
/// rather than a `/` catch-all (ADR-0128 × ADR-0131).
const ROUTED_STREAM_HANDLER_NAMESPACE: &str = "test.web_stream_routed";

/// The `WebSocketHandler` fixture's `NAMESPACE` const (ADR-0129).
const WS_HANDLER_NAMESPACE: &str = "test.web_socket";

/// RFC 6455 §1.3 worked-vector handshake key, and the `Sec-WebSocket-Accept`
/// the server must echo for it (base64(SHA-1(key + GUID))). Using the fixed
/// vector keeps the crypto out of the test — the cap's own tripwire pins the
/// computation.
const WS_TEST_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const WS_TEST_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// A second, distinct handshake key for the concurrent-connection case.
/// Its `Sec-WebSocket-Accept` isn't asserted — only that the connection
/// upgrades and gets its own per-connection greeting/echoes.
const WS_TEST_KEY_2: &str = "AQIDBAUGBwgJCgsMDQ4PEA==";

/// Slice of `response` after the HTTP head's blank line, or empty if absent.
fn body_after_head(response: &[u8]) -> &[u8] {
    response.windows(4).position(|w| w == b"\r\n\r\n").map_or(&[][..], |i| &response[i + 4..])
}

/// Why a chunked body failed to reassemble. Every variant is a framing
/// defect, distinct from a body that terminated early but legally — that
/// one reassembles fine and is compared by the caller.
#[derive(Debug)]
enum DechunkError {
    /// A chunk-size line that isn't hex. Separating this from the
    /// legitimate `0` terminator is the point: treating it as `0` ends
    /// reassembly and reports a malformed body as a short one.
    UnparsableSize { line: Vec<u8> },
    /// A chunk whose declared length runs past the bytes actually present
    /// — the response was cut mid-chunk.
    TruncatedChunk { declared: usize, available: usize },
    /// The body ran out before the zero-length terminating chunk. Covers an
    /// empty body and a tail carrying no chunk header at all, both of which
    /// would otherwise reassemble into a plausible-looking short payload.
    Unterminated { reassembled: usize, trailing: Vec<u8> },
}

impl fmt::Display for DechunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparsableSize { line } => {
                write!(f, "unparsable chunk-size line {:?}", String::from_utf8_lossy(line))
            }
            Self::TruncatedChunk { declared, available } => {
                write!(f, "chunk declares {declared} bytes but only {available} remain")
            }
            Self::Unterminated { reassembled, trailing } => write!(
                f,
                "body ended after {reassembled} reassembled bytes with no terminating chunk; trailing {:?}",
                String::from_utf8_lossy(trailing),
            ),
        }
    }
}

/// Reassemble a chunked transfer-encoding body into its payload, stopping at
/// the zero-length terminating chunk.
///
/// A body this cannot parse is an error rather than a short read: the caller
/// decides whether that is fatal (an assertion site) or expected (a poll loop
/// still waiting on asynchronous route registration). The terminating chunk
/// is required, so an empty or header-less body is an error too rather than a
/// successful reassembly of nothing.
fn dechunk(body: &[u8]) -> Result<Vec<u8>, DechunkError> {
    let mut pos = 0;
    let mut out = Vec::new();
    loop {
        let unterminated = |out: &Vec<u8>| DechunkError::Unterminated {
            reassembled: out.len(),
            trailing: body[pos.min(body.len())..].to_vec(),
        };
        if pos >= body.len() {
            return Err(unterminated(&out));
        }
        let Some(crlf) = (pos..body.len().saturating_sub(1)).find(|&i| body[i] == b'\r' && body[i + 1] == b'\n') else {
            return Err(unterminated(&out));
        };
        let line = &body[pos..crlf];
        let Some(size) = from_utf8(line).ok().and_then(|s| usize::from_str_radix(s.trim(), 16).ok()) else {
            return Err(DechunkError::UnparsableSize { line: line.to_vec() });
        };
        pos = crlf + 2;
        if size == 0 {
            return Ok(out);
        }
        if pos + size > body.len() {
            return Err(DechunkError::TruncatedChunk { declared: size, available: body.len() - pos });
        }
        out.extend_from_slice(&body[pos..pos + size]);
        pos += size + 2;
    }
}

/// How long `round_trip` waits for the server to finish a response before
/// giving up on the read.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// One HTTP exchange: the bytes read, plus how the read ended.
struct Exchange {
    bytes: Vec<u8>,
    /// The read stopped at [`READ_TIMEOUT`] rather than at the server's
    /// EOF, so the response is however much arrived before the deadline —
    /// not a complete short response.
    timed_out: bool,
}

impl Exchange {
    /// The response bytes, asserting the server actually closed the
    /// connection. A timeout means it never finished the response, which is
    /// a defect in the code under test rather than a shorter body to
    /// compare against — so it fails here, naming itself, instead of
    /// surfacing downstream as a content mismatch.
    fn complete(self) -> Vec<u8> {
        assert!(
            !self.timed_out,
            "read timed out after {}s with {} bytes and no EOF; server never finished the response: {:?}",
            READ_TIMEOUT.as_secs(),
            self.bytes.len(),
            String::from_utf8_lossy(&self.bytes),
        );
        self.bytes
    }
}

/// Write the raw HTTP/1.1 `request` to `port` on loopback, read until EOF
/// (the cap sends `Connection: close`) or [`READ_TIMEOUT`], and return the
/// raw response bytes alongside which of the two ended the read. Mirrors
/// the helper used in the `http_server.rs` cap unit tests.
fn round_trip(port: u16, request: &[u8]) -> Exchange {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(READ_TIMEOUT)).expect("set_read_timeout");
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush request");

    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut timed_out = false;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                timed_out = true;
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // A reset peer is still an end-of-response, not a deadline miss.
            Err(_) => break,
        }
    }
    Exchange { bytes: response, timed_out }
}

/// RFC 6455 opcodes the websocket e2e client uses.
const WS_OPCODE_TEXT: u8 = 0x1;
const WS_OPCODE_CONTINUATION: u8 = 0x0;
const WS_OPCODE_CLOSE: u8 = 0x8;

/// Serialize a masked client→server frame (RFC 6455 §5.1 requires client
/// frames be masked). `fin` marks the final fragment.
fn ws_client_frame(opcode: u8, payload: &[u8], fin: bool) -> Vec<u8> {
    let mask = [0x12u8, 0x34, 0x56, 0x78];
    let mut out = Vec::with_capacity(payload.len() + 6);
    let fin_bit = if fin {
        0x80
    } else {
        0x00
    };
    out.push(fin_bit | opcode);
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | u8::try_from(len).unwrap_or(0));
    } else if let Ok(short) = u16::try_from(len) {
        out.push(0x80 | 0x7E);
        out.extend_from_slice(&short.to_be_bytes());
    } else {
        out.push(0x80 | 0x7F);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, byte) in payload.iter().enumerate() {
        out.push(byte ^ mask[i % 4]);
    }
    out
}

/// Read exactly `n` bytes off the socket (the e2e client controls the timing,
/// so a short read means a bug, not partial data).
fn read_exact_n(stream: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).expect("read exact frame bytes");
    buf
}

/// Read one server→client frame (unmasked, RFC 6455 §5.1) and return its
/// opcode + payload.
fn read_server_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let header = read_exact_n(stream, 2);
    let opcode = header[0] & 0x0F;
    assert_eq!(header[1] & 0x80, 0, "a server→client frame must be unmasked");
    let len = match header[1] & 0x7F {
        126 => {
            let ext = read_exact_n(stream, 2);
            usize::from(u16::from_be_bytes([ext[0], ext[1]]))
        }
        127 => {
            let ext = read_exact_n(stream, 8);
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&ext);
            usize::try_from(u64::from_be_bytes(arr)).unwrap_or(0)
        }
        n => usize::from(n),
    };
    let payload = read_exact_n(stream, len);
    (opcode, payload)
}

/// Read the HTTP response head (through the blank line) one byte at a time, so
/// the read stops exactly at the head boundary and does not consume any
/// following websocket frame bytes.
fn read_http_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("read head byte");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

mod tests {
    use super::*;

    /// Boot a headless chassis with `HttpServerCapability` bound (port 0,
    /// OS picks) and the `http_handler` wasm fixture loaded via the autoload
    /// path. Once the guest trampoline is live, send two real HTTP/1.1
    /// requests over a `TcpStream` and assert:
    ///
    /// - `GET /` → `200 OK` echoing `/` in the body
    /// - `GET /missing` → `200 OK` echoing `/missing` in the body — a
    ///   non-root path serves a success body rather than a `404`
    ///   (tripwire for issue 2603 finding 2).
    #[test]
    fn wasm_handler_serves_http_requests() {
        let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
        let Some(wasm_path) = locate_component_wasm("aether_test_fixtures_bundle") else {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but http_handler.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
            );
            eprintln!(
                "skipping: http_handler.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown \
                 -p aether-test-fixtures-bundle`",
            );
            return;
        };
        let wasm = fs::read(&wasm_path).expect("read http_handler wasm");

        let server_config = HttpServerConfig {
            enabled: true,
            bind_addr: "127.0.0.1:0".to_string(),
            max_request_bytes: 65_536,
            max_header_bytes: 8_192,
            request_timeout_millis: 10_000,
            keep_alive_timeout_millis: 5_000,
            max_connections: 1024,
            response_stream_window: 16,
            request_stream_window: 16,
            dispatch_shards: 0,
            websocket_idle_timeout_millis: 300_000,
        };

        let sandbox = init_save_sandbox("http-serving");
        let env = HeadlessEnv {
            namespace_roots: test_namespace_roots(sandbox),
            http: HttpConfig::default(),
            http_server: Some(server_config),
            anthropic: AnthropicConfig::default(),
            gemini: GeminiConfig::default(),
            contentgen: ContentGenConfig::default(),
            tick_period: Duration::from_millis(100),
            rpc_addr: None,
            workers: None,
            ring_caps: aether_substrate_bundle::RingCapacities::default(),
            scheduler_tuning: aether_substrate_bundle::SchedulerTuning::default(),
            teardown_cap: Duration::from_millis(100),
            lifecycle_advance_timeout_millis: 1_000,
            autoload: vec![AutoloadComponent {
                wasm,
                config: Vec::new(),
                name: Some(HANDLER_NAMESPACE.to_owned()),
                // `HttpHandler` is a non-entry actor in the bundle.
                export: Some(HANDLER_NAMESPACE.to_owned()),
            }],
        };

        let built = HeadlessChassis::build(env).expect("build headless chassis with http server");

        // Wait for the wasm handler trampoline to come up.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if built.resolve_actor::<WasmTrampoline>(HANDLER_NAMESPACE).is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "http_handler trampoline did not register within 30s; \
                 live trampolines: {:?}",
                built.resolve_actors::<WasmTrampoline>(),
            );
            thread::sleep(Duration::from_millis(25));
        }

        // Retrieve the OS-assigned port from the published handle.
        let port =
            built.handle::<HttpServerHandle>().expect("HttpServerHandle published by HttpServerCapability").local_port;
        assert!(port > 0, "bound to an OS-assigned port");

        // The handler binds the `/` catch-all via async `wire` mail, so poll
        // it live before the assertions rather than racing the registration.
        poll_body_contains(
            port,
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            "hello from aether",
        );

        // GET / → 200 with body "hello from aether"
        let root_response =
            round_trip(port, b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").complete();
        let root_str = String::from_utf8_lossy(&root_response);
        assert!(root_str.starts_with("HTTP/1.1 200 "), "GET / should reply 200, got: {root_str:?}");
        assert!(
            root_str.contains("hello from aether"),
            "GET / body should contain 'hello from aether', got: {root_str:?}",
        );

        // GET /missing → 200, echoing the path (a non-root path serves
        // success, not a 404 trap).
        let miss_response =
            round_trip(port, b"GET /missing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").complete();
        let miss_str = String::from_utf8_lossy(&miss_response);
        assert!(miss_str.starts_with("HTTP/1.1 200 "), "GET /missing should reply 200, got: {miss_str:?}");
        assert!(miss_str.contains("/missing"), "GET /missing body should echo the request path, got: {miss_str:?}");
    }

    /// Boot a headless chassis with `HttpServerCapability` bound and the
    /// `StreamingHttpHandler` wasm fixture loaded (ADR-0128). Fire one real
    /// HTTP/1.1 request over a `TcpStream` and assert the response is a
    /// `200` chunked stream the client reassembles into every emitted chunk
    /// in order — the real-wire proof that a wasm handler streams a
    /// multi-chunk body under the credit protocol.
    #[test]
    fn wasm_handler_streams_chunked_response() {
        let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
        let Some(wasm_path) = locate_component_wasm("aether_test_fixtures_bundle") else {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but http_handler.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
            );
            eprintln!(
                "skipping: http_handler.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown \
                 -p aether-test-fixtures-bundle`",
            );
            return;
        };
        let wasm = fs::read(&wasm_path).expect("read http_handler wasm");

        let server_config = HttpServerConfig {
            enabled: true,
            bind_addr: "127.0.0.1:0".to_string(),
            max_request_bytes: 65_536,
            max_header_bytes: 8_192,
            request_timeout_millis: 10_000,
            keep_alive_timeout_millis: 5_000,
            max_connections: 1024,
            // Window below the chunk count so the round trip must replenish.
            response_stream_window: 8,
            request_stream_window: 16,
            dispatch_shards: 0,
            websocket_idle_timeout_millis: 300_000,
        };

        let sandbox = init_save_sandbox("http-serving-stream");
        let env = HeadlessEnv {
            namespace_roots: test_namespace_roots(sandbox),
            http: HttpConfig::default(),
            http_server: Some(server_config),
            anthropic: AnthropicConfig::default(),
            gemini: GeminiConfig::default(),
            contentgen: ContentGenConfig::default(),
            tick_period: Duration::from_millis(100),
            rpc_addr: None,
            workers: None,
            ring_caps: aether_substrate_bundle::RingCapacities::default(),
            scheduler_tuning: aether_substrate_bundle::SchedulerTuning::default(),
            teardown_cap: Duration::from_millis(100),
            lifecycle_advance_timeout_millis: 1_000,
            autoload: vec![AutoloadComponent {
                wasm,
                config: Vec::new(),
                name: Some(STREAM_HANDLER_NAMESPACE.to_owned()),
                export: Some(STREAM_HANDLER_NAMESPACE.to_owned()),
            }],
        };

        let built = HeadlessChassis::build(env).expect("build headless chassis with http server");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if built.resolve_actor::<WasmTrampoline>(STREAM_HANDLER_NAMESPACE).is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "streaming trampoline did not register within 30s; \
                 live trampolines: {:?}",
                built.resolve_actors::<WasmTrampoline>(),
            );
            thread::sleep(Duration::from_millis(25));
        }

        let port =
            built.handle::<HttpServerHandle>().expect("HttpServerHandle published by HttpServerCapability").local_port;
        assert!(port > 0, "bound to an OS-assigned port");

        // The handler binds the `/` catch-all via async `wire` mail, so poll
        // it live before the assertions rather than racing the registration.
        poll_body_contains(port, b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", "chunk-");

        let response =
            round_trip(port, b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").complete();
        let head = String::from_utf8_lossy(&response);
        assert!(head.starts_with("HTTP/1.1 200 "), "GET /stream should reply 200, got: {head:?}");
        assert!(head.contains("Transfer-Encoding: chunked"), "streamed response should be chunked, got: {head:?}");

        let reassembled = dechunk(body_after_head(&response)).expect("reassemble the streamed body");
        let expected: Vec<u8> = (0..STREAM_CHUNK_COUNT).flat_map(|i| format!("chunk-{i}\n").into_bytes()).collect();
        assert_eq!(reassembled, expected, "reassembled stream body: {:?}", String::from_utf8_lossy(&reassembled));
    }

    /// Regression for issue 2600: a response-streaming handler reached
    /// through an externally-registered route (ADR-0131) must still be granted
    /// its initial response-stream credit window (ADR-0128), so it emits its
    /// body chunks rather than opening the stream and hanging (`curl` error 18).
    ///
    /// The `RoutedStreamingHttpHandler` fixture claims only `/routed-stream`
    /// for its own mailbox in `wire` and binds no `/` catch-all, so a
    /// `/routed-stream` request can only reach it via that specific route —
    /// the negative control is now inherent (no catch-all to mask the route
    /// path). On today's bug `open_stream` drops the initial credit grant, so
    /// zero chunks arrive; after the fix the grant reaches the route's handler
    /// and the client reassembles the same body the catch-all streaming test
    /// asserts.
    #[test]
    fn wasm_handler_streams_chunked_response_via_registered_route() {
        let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
        let Some(wasm_path) = locate_component_wasm("aether_test_fixtures_bundle") else {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but http_handler.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
            );
            eprintln!(
                "skipping: http_handler.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown \
                 -p aether-test-fixtures-bundle`",
            );
            return;
        };
        let wasm = fs::read(&wasm_path).expect("read http_handler wasm");

        let server_config = HttpServerConfig {
            enabled: true,
            bind_addr: "127.0.0.1:0".to_string(),
            // No catch-all: the streaming handler is reachable only through
            // the `/routed-stream` route it registers, so nothing can mask
            // the bug (issue 2600 negative control).
            max_request_bytes: 65_536,
            max_header_bytes: 8_192,
            request_timeout_millis: 10_000,
            keep_alive_timeout_millis: 5_000,
            max_connections: 1024,
            // Window below the chunk count so the round trip must replenish.
            response_stream_window: 8,
            request_stream_window: 16,
            dispatch_shards: 0,
            websocket_idle_timeout_millis: 300_000,
        };

        let sandbox = init_save_sandbox("http-serving-stream-route");
        let env = HeadlessEnv {
            namespace_roots: test_namespace_roots(sandbox),
            http: HttpConfig::default(),
            http_server: Some(server_config),
            anthropic: AnthropicConfig::default(),
            gemini: GeminiConfig::default(),
            contentgen: ContentGenConfig::default(),
            tick_period: Duration::from_millis(100),
            rpc_addr: None,
            workers: None,
            ring_caps: aether_substrate_bundle::RingCapacities::default(),
            scheduler_tuning: aether_substrate_bundle::SchedulerTuning::default(),
            teardown_cap: Duration::from_millis(100),
            lifecycle_advance_timeout_millis: 1_000,
            autoload: vec![AutoloadComponent {
                wasm,
                config: Vec::new(),
                name: Some(ROUTED_STREAM_HANDLER_NAMESPACE.to_owned()),
                export: Some(ROUTED_STREAM_HANDLER_NAMESPACE.to_owned()),
            }],
        };

        let built = HeadlessChassis::build(env).expect("build headless chassis with http server");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if built.resolve_actor::<WasmTrampoline>(ROUTED_STREAM_HANDLER_NAMESPACE).is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "routed streaming trampoline did not register within 30s; \
                 live trampolines: {:?}",
                built.resolve_actors::<WasmTrampoline>(),
            );
            thread::sleep(Duration::from_millis(25));
        }

        let port =
            built.handle::<HttpServerHandle>().expect("HttpServerHandle published by HttpServerCapability").local_port;
        assert!(port > 0, "bound to an OS-assigned port");

        // Route registration is async mail — poll until the streamed body
        // reassembles rather than racing the `register_route_self`.
        let expected: Vec<u8> = (0..STREAM_CHUNK_COUNT).flat_map(|i| format!("chunk-{i}\n").into_bytes()).collect();
        let request = b"GET /routed-stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let response = round_trip(port, request).bytes;
            let head = String::from_utf8_lossy(&response);
            if head.starts_with("HTTP/1.1 200 ")
                && head.contains("Transfer-Encoding: chunked")
                && dechunk(body_after_head(&response)).is_ok_and(|body| body == expected)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "routed streaming response did not reassemble within 30s; last: {head:?}",
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Boot a headless chassis with `HttpServerCapability` bound and the
    /// `WebSocketHandler` wasm fixture loaded (ADR-0129 / ADR-0132). Over a
    /// real `TcpStream`: perform the RFC 6455 upgrade handshake, read the
    /// fixture's unsolicited greeting (stream-id-addressed push with no
    /// inbound message in flight — the ADR-0132 case chain-root routing could
    /// not serve), exchange a message each direction (a single frame, then a
    /// fragmented message to exercise continuation reassembly), prove an
    /// unknown-stream send is dropped without teardown, and close cleanly —
    /// the end-to-end proof that a wasm handler serves a live bidirectional
    /// websocket. Also opens a second, concurrent connection and asserts it
    /// gets its own greeting and its own echoes with no cross-talk against
    /// the first connection — the tripwire for issue 2603 finding 1 (the
    /// fixture used to key its greeted flag and captured stream per actor,
    /// not per connection).
    #[test]
    #[allow(clippy::too_many_lines)]
    fn wasm_handler_serves_a_websocket_round_trip() {
        let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
        let Some(wasm_path) = locate_component_wasm("aether_test_fixtures_bundle") else {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but http_handler.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
            );
            eprintln!(
                "skipping: http_handler.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown \
                 -p aether-test-fixtures-bundle`",
            );
            return;
        };
        let wasm = fs::read(&wasm_path).expect("read http_handler wasm");

        let server_config = HttpServerConfig {
            enabled: true,
            bind_addr: "127.0.0.1:0".to_string(),
            max_request_bytes: 65_536,
            max_header_bytes: 8_192,
            request_timeout_millis: 10_000,
            keep_alive_timeout_millis: 5_000,
            max_connections: 1024,
            response_stream_window: 8,
            request_stream_window: 16,
            dispatch_shards: 0,
            websocket_idle_timeout_millis: 10_000,
        };

        let sandbox = init_save_sandbox("http-serving-websocket");
        let env = HeadlessEnv {
            namespace_roots: test_namespace_roots(sandbox),
            http: HttpConfig::default(),
            http_server: Some(server_config),
            anthropic: AnthropicConfig::default(),
            gemini: GeminiConfig::default(),
            contentgen: ContentGenConfig::default(),
            tick_period: Duration::from_millis(100),
            rpc_addr: None,
            workers: None,
            ring_caps: aether_substrate_bundle::RingCapacities::default(),
            scheduler_tuning: aether_substrate_bundle::SchedulerTuning::default(),
            teardown_cap: Duration::from_millis(100),
            lifecycle_advance_timeout_millis: 1_000,
            autoload: vec![AutoloadComponent {
                wasm,
                config: Vec::new(),
                name: Some(WS_HANDLER_NAMESPACE.to_owned()),
                export: Some(WS_HANDLER_NAMESPACE.to_owned()),
            }],
        };

        let built = HeadlessChassis::build(env).expect("build headless chassis with http server");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if built.resolve_actor::<WasmTrampoline>(WS_HANDLER_NAMESPACE).is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "websocket trampoline did not register within 30s; \
                 live trampolines: {:?}",
                built.resolve_actors::<WasmTrampoline>(),
            );
            thread::sleep(Duration::from_millis(25));
        }

        let port =
            built.handle::<HttpServerHandle>().expect("HttpServerHandle published by HttpServerCapability").local_port;
        assert!(port > 0, "bound to an OS-assigned port");

        // Handshake. The handler binds the `/` catch-all via async `wire`
        // mail, so poll the upgrade live (reconnecting each attempt) rather
        // than racing the registration — a pre-registration upgrade is 503.
        let handshake = format!(
            "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: {WS_TEST_KEY}\r\n\r\n"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut stream, head) = loop {
            let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
            stream.set_read_timeout(Some(Duration::from_secs(10))).expect("set_read_timeout");
            stream.write_all(handshake.as_bytes()).expect("write handshake");
            stream.flush().expect("flush handshake");
            let head = read_http_head(&mut stream);
            if head.starts_with("HTTP/1.1 101 ") {
                break (stream, head);
            }
            assert!(
                Instant::now() < deadline,
                "websocket upgrade did not go live (route registration) within 30s; last: {head:?}",
            );
            thread::sleep(Duration::from_millis(25));
        };
        assert!(
            head.contains(&format!("Sec-WebSocket-Accept: {WS_TEST_ACCEPT}")),
            "101 should echo the computed accept key, got: {head:?}",
        );

        // The fixture pushes an unsolicited greeting on its first credit
        // grant (ADR-0132) — read it before sending anything: outbound
        // routing holds with no inbound websocket message in flight.
        let (opcode, payload) = read_server_frame(&mut stream);
        assert_eq!(opcode, WS_OPCODE_TEXT, "greeting should be a text frame");
        assert_eq!(payload, b"server greeting", "the fixture's unsolicited push should arrive before any client frame");

        // Concurrent second connection: the fixture keys its greeting +
        // captured stream per connection (`stream_id`), not per actor, so a
        // second concurrent client must get its own greeting and its own
        // echoes with no cross-talk against the first connection (issue
        // 2603 finding 1).
        let mut stream2 = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect second ws client");
        stream2.set_read_timeout(Some(Duration::from_secs(10))).expect("set_read_timeout for second connection");
        let handshake2 = format!(
            "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: {WS_TEST_KEY_2}\r\n\r\n"
        );
        stream2.write_all(handshake2.as_bytes()).expect("write second handshake");
        stream2.flush().expect("flush second handshake");

        let head2 = read_http_head(&mut stream2);
        assert!(head2.starts_with("HTTP/1.1 101 "), "second connection's upgrade should reply 101, got: {head2:?}");

        // Its own accept-time greeting, distinct from the first
        // connection's — proves the greet-once behavior is per connection.
        let (opcode, payload) = read_server_frame(&mut stream2);
        assert_eq!(opcode, WS_OPCODE_TEXT, "second connection's greeting should be a text frame");
        assert_eq!(payload, b"server greeting", "the second connection should get its own greeting");

        // Interleave a send on each connection, then read the echoes back
        // in reverse order: if the fixture still tracked a single captured
        // stream instead of a per-connection map, one of these echoes
        // would either go missing or land on the wrong socket.
        stream.write_all(&ws_client_frame(WS_OPCODE_TEXT, b"hello from conn1", true)).expect("write conn1 frame");
        stream.flush().expect("flush conn1 frame");
        stream2.write_all(&ws_client_frame(WS_OPCODE_TEXT, b"hello from conn2", true)).expect("write conn2 frame");
        stream2.flush().expect("flush conn2 frame");

        let (opcode2, payload2) = read_server_frame(&mut stream2);
        assert_eq!(opcode2, WS_OPCODE_TEXT, "conn2's echo should be a text frame");
        assert_eq!(payload2, b"hello from conn2", "conn2's echo must not cross-talk with conn1");

        let (opcode1, payload1) = read_server_frame(&mut stream);
        assert_eq!(opcode1, WS_OPCODE_TEXT, "conn1's echo should be a text frame");
        assert_eq!(payload1, b"hello from conn1", "conn1's echo must not cross-talk with conn2");

        // Close the second connection cleanly so it doesn't linger for the
        // rest of the single-connection assertions below.
        let mut close_payload2 = Vec::new();
        close_payload2.extend_from_slice(&1000u16.to_be_bytes());
        stream2.write_all(&ws_client_frame(WS_OPCODE_CLOSE, &close_payload2, true)).expect("write second close frame");
        stream2.flush().expect("flush second close frame");
        let (opcode, _payload) = read_server_frame(&mut stream2);
        assert_eq!(opcode, WS_OPCODE_CLOSE, "the cap should echo a close frame for the second connection");

        // A single-frame message echoes back verbatim.
        stream.write_all(&ws_client_frame(WS_OPCODE_TEXT, b"hello websocket", true)).expect("write text frame");
        stream.flush().expect("flush text frame");
        let (opcode, payload) = read_server_frame(&mut stream);
        assert_eq!(opcode, WS_OPCODE_TEXT, "echo should be a text frame");
        assert_eq!(payload, b"hello websocket", "echo should match what was sent");

        // A fragmented message (two frames) reassembles into one echoed message
        // — the cap reassembles inbound continuation frames before dispatch.
        stream.write_all(&ws_client_frame(WS_OPCODE_TEXT, b"frag-", false)).expect("write fragment 1");
        stream.write_all(&ws_client_frame(WS_OPCODE_CONTINUATION, b"ment", true)).expect("write fragment 2");
        stream.flush().expect("flush fragments");
        let (opcode, payload) = read_server_frame(&mut stream);
        assert_eq!(opcode, WS_OPCODE_TEXT, "reassembled echo is a text frame");
        assert_eq!(payload, b"frag-ment", "the cap should reassemble the fragmented message before dispatch");

        // An outbound send naming an unknown stream id is dropped without
        // tearing the connection down (ADR-0132): the fixture echoes
        // `misroute` to a deliberately wrong stream id, so the next frame the
        // client reads must be the echo of the following message — server
        // writes are ordered, so reading the second echo first proves the
        // misrouted frame never reached the socket, and reading it at all
        // proves the connection survived the drop.
        stream.write_all(&ws_client_frame(WS_OPCODE_TEXT, b"misroute", true)).expect("write misroute frame");
        stream.write_all(&ws_client_frame(WS_OPCODE_TEXT, b"after misroute", true)).expect("write post-misroute frame");
        stream.flush().expect("flush misroute frames");
        let (opcode, payload) = read_server_frame(&mut stream);
        assert_eq!(opcode, WS_OPCODE_TEXT, "post-misroute echo is a text frame");
        assert_eq!(payload, b"after misroute", "the misrouted echo must be dropped and the connection must survive");

        // Clean close: send a close frame, read the cap's echoed close.
        let mut close_payload = Vec::new();
        close_payload.extend_from_slice(&1000u16.to_be_bytes());
        stream.write_all(&ws_client_frame(WS_OPCODE_CLOSE, &close_payload, true)).expect("write close frame");
        stream.flush().expect("flush close frame");
        let (opcode, _payload) = read_server_frame(&mut stream);
        assert_eq!(opcode, WS_OPCODE_CLOSE, "the cap should echo a close frame");
    }

    /// Poll `request` until the response body contains `expected`
    /// (bounded deadline): route registration, and later the drop's
    /// route purge, are asynchronous mail the test must not race.
    fn poll_body_contains(port: u16, request: &[u8], expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let response = round_trip(port, request).bytes;
            let text = String::from_utf8_lossy(&response);
            if text.split_once("\r\n\r\n").is_some_and(|(_, body)| body.contains(expected)) {
                return;
            }
            assert!(Instant::now() < deadline, "expected body containing {expected:?} within 30s; last: {text:?}");
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// ADR-0130 drop-purge, end to end over real wasm: a guest claims
    /// `/routed` via `register_route_self` in `wire`; dropping the
    /// component (`aether.component.drop` → the component cap's
    /// `unregister_routes_all` fan-out) purges the route, so the same
    /// request falls back to the `test.web` fixture's `/` catch-all. The
    /// drop is injected through the routed guest itself — the request
    /// body names the trampoline mailbox id to drop — since the built
    /// chassis exposes no direct mail surface to the test.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn dropped_component_routes_are_purged() {
        const ROUTED_NAMESPACE: &str = "test.routed_web";
        let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
        let Some(wasm_path) = locate_component_wasm("aether_test_fixtures_bundle") else {
            assert!(
                !strict,
                "AETHER_REQUIRE_RUNTIME set but http_handler.wasm not pre-built; \
                 CI's `Pre-build component wasm for scenario tests` step is missing it",
            );
            eprintln!(
                "skipping: http_handler.wasm not built; \
                 run `cargo build --target wasm32-unknown-unknown \
                 -p aether-test-fixtures-bundle`",
            );
            return;
        };
        let wasm = fs::read(&wasm_path).expect("read http_handler wasm");

        let server_config = HttpServerConfig {
            enabled: true,
            bind_addr: "127.0.0.1:0".to_string(),
            max_request_bytes: 65_536,
            max_header_bytes: 8_192,
            request_timeout_millis: 10_000,
            keep_alive_timeout_millis: 5_000,
            max_connections: 1024,
            response_stream_window: 16,
            request_stream_window: 16,
            dispatch_shards: 0,
            websocket_idle_timeout_millis: 300_000,
        };

        let sandbox = init_save_sandbox("http-route-drop");
        let env = HeadlessEnv {
            namespace_roots: test_namespace_roots(sandbox),
            http: HttpConfig::default(),
            http_server: Some(server_config),
            anthropic: AnthropicConfig::default(),
            gemini: GeminiConfig::default(),
            contentgen: ContentGenConfig::default(),
            tick_period: Duration::from_millis(100),
            rpc_addr: None,
            workers: None,
            ring_caps: aether_substrate_bundle::RingCapacities::default(),
            scheduler_tuning: aether_substrate_bundle::SchedulerTuning::default(),
            teardown_cap: Duration::from_millis(100),
            lifecycle_advance_timeout_millis: 1_000,
            autoload: vec![
                AutoloadComponent {
                    wasm: wasm.clone(),
                    config: Vec::new(),
                    name: Some(HANDLER_NAMESPACE.to_owned()),
                    export: Some(HANDLER_NAMESPACE.to_owned()),
                },
                AutoloadComponent {
                    wasm,
                    config: Vec::new(),
                    name: Some(ROUTED_NAMESPACE.to_owned()),
                    export: Some(ROUTED_NAMESPACE.to_owned()),
                },
            ],
        };

        let built = HeadlessChassis::build(env).expect("build headless chassis with http server");

        // Wait for both trampolines (fallback + routed guest).
        let deadline = Instant::now() + Duration::from_secs(30);
        let routed_mailbox = loop {
            if let Some(routed) = built.resolve_actor::<WasmTrampoline>(ROUTED_NAMESPACE)
                && built.resolve_actor::<WasmTrampoline>(HANDLER_NAMESPACE).is_some()
            {
                break routed;
            }
            assert!(
                Instant::now() < deadline,
                "trampolines did not register within 30s; live: {:?}",
                built.resolve_actors::<WasmTrampoline>(),
            );
            thread::sleep(Duration::from_millis(25));
        };

        let port =
            built.handle::<HttpServerHandle>().expect("HttpServerHandle published by HttpServerCapability").local_port;

        // Route live (registration is async mail — poll, don't race).
        poll_body_contains(
            port,
            b"GET /routed HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            "routed handler",
        );

        // `/routed/drop` is a second exact route (#3697) with its own async
        // registration — confirm it live before the destructive POST so that
        // request cannot race its registration. An empty body is a clean 400
        // ("decimal mailbox id"), so this probe drops nothing.
        poll_body_contains(
            port,
            b"GET /routed/drop HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            "decimal mailbox id",
        );

        // Drop the routed component through the guest bridge: the body
        // carries the trampoline mailbox id to drop.
        let drop_body = routed_mailbox.0.to_string();
        let drop_request = format!(
            "POST /routed/drop HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{}",
            drop_body.len(),
            drop_body,
        );
        let drop_response = round_trip(port, drop_request.as_bytes()).complete();
        let drop_str = String::from_utf8_lossy(&drop_response);
        assert!(
            drop_str.starts_with("HTTP/1.1 200 ") && drop_str.contains("dropping"),
            "drop bridge should acknowledge, got: {drop_str:?}",
        );

        // The purge rides the drop fan-out; once it lands, /routed falls
        // back to the `web` fixture, which echoes the path in its 200 body
        // — distinct from the routed component's fixed "routed handler"
        // body, so this still discriminates route-live from route-purged.
        poll_body_contains(
            port,
            b"GET /routed HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            "hello from aether: /routed",
        );
    }
}
