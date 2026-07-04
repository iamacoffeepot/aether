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
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::str::from_utf8;
use std::thread;
use std::time::{Duration, Instant};

use aether_substrate_bundle::Chassis as _;
use aether_substrate_bundle::autoload::AutoloadComponent;
use aether_substrate_bundle::capabilities::http::HttpConfig;
use aether_substrate_bundle::capabilities::{
    AnthropicConfig, ContentGenConfig, GeminiConfig, HttpServerConfig, HttpServerHandle,
    WasmTrampoline,
};
use aether_substrate_bundle::headless::{HeadlessChassis, HeadlessEnv};
use aether_substrate_bundle::test_bench::test_helpers::{
    init_save_sandbox, locate_component_wasm, test_namespace_roots,
};

/// The `http_handler` fixture's `NAMESPACE` const — the subname under
/// which `WasmTrampoline` registers it, and the last segment of its
/// full lineage address (`aether.component/aether.embedded:web`).
const HANDLER_NAMESPACE: &str = "web";

/// The full handler mailbox address the http server cap resolves at
/// dispatch time (ADR-0108 §3 late binding).
const HANDLER_MAILBOX: &str = "aether.component/aether.embedded:web";

/// The `StreamingHttpHandler` fixture's `NAMESPACE` const (ADR-0128).
const STREAM_HANDLER_NAMESPACE: &str = "web_stream";

/// The streaming handler's full lineage mailbox address.
const STREAM_HANDLER_MAILBOX: &str = "aether.component/aether.embedded:web_stream";

/// The chunk count `StreamingHttpHandler` emits — kept in step with the
/// fixture's own `STREAM_CHUNK_COUNT`.
const STREAM_CHUNK_COUNT: u32 = 20;

/// Slice of `response` after the HTTP head's blank line, or empty if absent.
fn body_after_head(response: &[u8]) -> &[u8] {
    response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(&[][..], |i| &response[i + 4..])
}

/// Reassemble a chunked transfer-encoding body into its payload, stopping at
/// the zero-length terminating chunk.
fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < body.len() {
        let Some(crlf) =
            (pos..body.len().saturating_sub(1)).find(|&i| body[i] == b'\r' && body[i + 1] == b'\n')
        else {
            break;
        };
        let size = from_utf8(&body[pos..crlf])
            .ok()
            .and_then(|s| usize::from_str_radix(s.trim(), 16).ok())
            .unwrap_or(0);
        pos = crlf + 2;
        if size == 0 || pos + size > body.len() {
            break;
        }
        out.extend_from_slice(&body[pos..pos + size]);
        pos += size + 2;
    }
    out
}

/// Write the raw HTTP/1.1 `request` to `port` on loopback, read until
/// EOF (the cap sends `Connection: close`), and return the raw response
/// bytes. Mirrors the helper used in the `http_server.rs` cap unit tests.
fn round_trip(port: u16, request: &[u8]) -> Vec<u8> {
    let mut stream =
        TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set_read_timeout");
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush request");

    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    response
}

mod tests {
    use super::*;

    /// Boot a headless chassis with `HttpServerCapability` bound (port 0,
    /// OS picks) and the `http_handler` wasm fixture loaded via the autoload
    /// path. Once the guest trampoline is live, send two real HTTP/1.1
    /// requests over a `TcpStream` and assert:
    ///
    /// - `GET /` → `200 OK` with body `hello from aether`
    /// - `GET /missing` → `404 Not Found`
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
                 -p aether-test-fixtures --examples`",
            );
            return;
        };
        let wasm = fs::read(&wasm_path).expect("read http_handler wasm");

        let server_config = HttpServerConfig {
            enabled: true,
            bind_addr: "127.0.0.1:0".to_string(),
            handler_mailbox: HANDLER_MAILBOX.to_string(),
            max_request_bytes: 65_536,
            max_header_bytes: 8_192,
            request_timeout_millis: 10_000,
            max_connections: 1024,
            response_stream_window: 16,
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
            if built
                .resolve_actor::<WasmTrampoline>(HANDLER_NAMESPACE)
                .is_some()
            {
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
        let port = built
            .handle::<HttpServerHandle>()
            .expect("HttpServerHandle published by HttpServerCapability")
            .local_port;
        assert!(port > 0, "bound to an OS-assigned port");

        // GET / → 200 with body "hello from aether"
        let root_response = round_trip(
            port,
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        let root_str = String::from_utf8_lossy(&root_response);
        assert!(
            root_str.starts_with("HTTP/1.1 200 "),
            "GET / should reply 200, got: {root_str:?}",
        );
        assert!(
            root_str.contains("hello from aether"),
            "GET / body should contain 'hello from aether', got: {root_str:?}",
        );

        // GET /missing → 404
        let miss_response = round_trip(
            port,
            b"GET /missing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        let miss_str = String::from_utf8_lossy(&miss_response);
        assert!(
            miss_str.starts_with("HTTP/1.1 404 "),
            "GET /missing should reply 404, got: {miss_str:?}",
        );
        assert!(
            miss_str.contains("not found"),
            "GET /missing body should contain 'not found', got: {miss_str:?}",
        );
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
                 -p aether-test-fixtures --examples`",
            );
            return;
        };
        let wasm = fs::read(&wasm_path).expect("read http_handler wasm");

        let server_config = HttpServerConfig {
            enabled: true,
            bind_addr: "127.0.0.1:0".to_string(),
            handler_mailbox: STREAM_HANDLER_MAILBOX.to_string(),
            max_request_bytes: 65_536,
            max_header_bytes: 8_192,
            request_timeout_millis: 10_000,
            max_connections: 1024,
            // Window below the chunk count so the round trip must replenish.
            response_stream_window: 8,
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
            if built
                .resolve_actor::<WasmTrampoline>(STREAM_HANDLER_NAMESPACE)
                .is_some()
            {
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

        let port = built
            .handle::<HttpServerHandle>()
            .expect("HttpServerHandle published by HttpServerCapability")
            .local_port;
        assert!(port > 0, "bound to an OS-assigned port");

        let response = round_trip(
            port,
            b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        let head = String::from_utf8_lossy(&response);
        assert!(
            head.starts_with("HTTP/1.1 200 "),
            "GET /stream should reply 200, got: {head:?}",
        );
        assert!(
            head.contains("Transfer-Encoding: chunked"),
            "streamed response should be chunked, got: {head:?}",
        );

        let reassembled = dechunk(body_after_head(&response));
        let expected: Vec<u8> = (0..STREAM_CHUNK_COUNT)
            .flat_map(|i| format!("chunk-{i}\n").into_bytes())
            .collect();
        assert_eq!(
            reassembled,
            expected,
            "reassembled stream body: {:?}",
            String::from_utf8_lossy(&reassembled),
        );
    }
}
