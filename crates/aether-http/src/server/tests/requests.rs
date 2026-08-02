//! Request parsing, framing rejects and the rendered response head: the
//! method / path / query / body round trip, the size and framing guards
//! (`413` / `411` / `501`), the no-route and response-less settlement paths
//! (`503` / `502`), interim `100 Continue`, and HEAD body suppression.

use aether_substrate::chassis::builder::Builder;
use aether_substrate::testing::{TestChassis, fresh_substrate};
use aether_trace::TraceDispatchCapability;
use std::sync::Arc;

use crate::server::HttpServerCapability;

use super::handlers::{EchoHttpHandler, FixedBodyHttpHandler, SilentHttpHandler};
use super::support::{body_of, boot_buffered, config_for, port_of, round_trip, round_trip_live};

/// A GET round-trips to the handler and its reply returns as
/// well-formed HTTP/1.1, carrying the parsed path / query / method.
#[test]
fn get_round_trips_to_handler() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port_of(&chassis), b"GET /hello?name=ada HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200 status line, got: {response:?}");
    assert!(response.contains("x-aether-method: Get\r\n"), "{response:?}");
    assert!(response.contains("x-aether-path: /hello\r\n"), "{response:?}");
    assert!(response.contains("x-aether-query: name=ada\r\n"), "{response:?}");
    let peer_addr_header = response
        .lines()
        .find_map(|line| line.strip_prefix("x-aether-peer-addr: "))
        .map(|value| value.trim_end_matches('\r'))
        .expect("x-aether-peer-addr header present");
    assert!(
        peer_addr_header.starts_with("127.0.0.1:"),
        "expected the loopback client's address, got: {peer_addr_header:?}",
    );
    assert!(response.contains("Content-Length: 0\r\n"), "{response:?}");
    assert!(response.contains("Date: "), "{response:?}");
    assert!(response.contains("Connection: close\r\n"), "{response:?}");
}

/// A POST round-trips the body verbatim to the handler.
#[test]
fn post_round_trips_body() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(
        port_of(&chassis),
        b"POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200, got: {response:?}");
    assert!(response.contains("x-aether-method: Post\r\n"), "{response:?}");
    assert_eq!(body_of(&response), "hello", "body echoed verbatim");
}

/// An announced `Content-Length` past the body cap is answered
/// `413` before any dispatch.
#[test]
fn oversize_body_is_413() {
    let chassis = boot_buffered::<EchoHttpHandler>(8);

    let response =
        round_trip(port_of(&chassis), b"POST /big HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 413 "), "expected 413, got: {response:?}");
}

/// A websocket-upgrade request that also declares an oversized
/// `Content-Length` is answered `413` before any dispatch or buffered-body
/// allocation. A valid upgrade handshake always buffers (never streams),
/// so the body-size cap must apply to it the same as any other buffered
/// request rather than being skipped because `ws_key.is_some()`.
#[test]
fn oversize_body_on_ws_upgrade_is_413() {
    let chassis = boot_buffered::<EchoHttpHandler>(8);

    let response = round_trip(
        port_of(&chassis),
        b"GET /ws HTTP/1.1\r\n\
          Host: localhost\r\n\
          Upgrade: websocket\r\n\
          Connection: Upgrade\r\n\
          Sec-WebSocket-Version: 13\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Content-Length: 100\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 413 "), "expected 413, got: {response:?}");
}

/// A websocket-upgrade request that also carries the request-smuggling
/// framing shape (both `Content-Length` and `Transfer-Encoding`) is
/// answered `411` before any dispatch — the same framing reject the
/// non-upgrade path applies, not skipped because `ws_key.is_some()`.
///
/// Tripwire: on `origin/main` this returns non-411 because the framing
/// reject sits inside `if ws_key.is_none()` and never runs for a valid
/// upgrade handshake.
#[test]
fn smuggling_on_ws_upgrade_is_411() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    let response = round_trip(
        port_of(&chassis),
        b"GET /ws HTTP/1.1\r\n\
          Host: localhost\r\n\
          Upgrade: websocket\r\n\
          Connection: Upgrade\r\n\
          Sec-WebSocket-Version: 13\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Content-Length: 5\r\n\
          Transfer-Encoding: chunked\r\n\r\nhello",
    );
    assert!(response.starts_with("HTTP/1.1 411 "), "expected 411, got: {response:?}");
}

/// A non-enumerated method is answered `501` before any dispatch.
#[test]
fn unknown_method_is_501() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    let response = round_trip(port_of(&chassis), b"FROB /x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 501 "), "expected 501, got: {response:?}");
}

/// A request matching no route — no handler registered a catch-all — is
/// answered `503` (ADR-0130).
#[test]
fn no_handler_is_503() {
    let (registry, mailer) = fresh_substrate();
    // No handler actor is booted, so nothing registers a `/` catch-all —
    // every request matches no route.
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<HttpServerCapability>((), config_for(1024))
        .build_passive()
        .expect("server boots");

    let response = round_trip(port_of(&chassis), b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 503 "), "expected 503, got: {response:?}");
}

/// A handler that receives the request but never replies settles
/// into `502` via the settlement safety net.
#[test]
fn response_less_chain_is_502() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        // TraceDispatchCapability folds trace events into per-root
        // counters and fires settlement once a root drains; without it
        // the server's settlement subscription never wakes.
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<SilentHttpHandler>(())
        .with_actor_configured::<HttpServerCapability>((), config_for(1024))
        .build_passive()
        .expect("caps boot");

    // The silent handler binds `/` via async `wire` mail; poll past the
    // pre-registration `503` to the dispatched `502` the settlement net raises.
    let response = round_trip_live(port_of(&chassis), b"GET /drop HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 502 "), "expected 502, got: {response:?}");
}

/// A percent-encoded path is decoded before it reaches the handler
/// (ADR-0108 §2's "the decoded path component"); the query string stays
/// raw.
#[test]
fn percent_encoded_path_is_decoded() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    // First request against the async-registered `/` catch-all: poll it live.
    let response = round_trip_live(port_of(&chassis), b"GET /hello%20world?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200, got: {response:?}");
    assert!(response.contains("x-aether-path: /hello world\r\n"), "{response:?}");
    assert!(response.contains("x-aether-query: x=1\r\n"), "{response:?}");
}

/// A `Transfer-Encoding: chunked` request to a *buffered* handler is rejected
/// `411`: an unknown-length body has nothing to buffer under (ADR-0128 relaxes
/// this only for a streaming handler, whose accept-set opts it into the
/// incremental path — see `chunked_upload_streams_to_streaming_handler`).
#[test]
fn transfer_encoding_is_411() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);

    let response = round_trip(
        port_of(&chassis),
        b"POST /chunked HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 411 "), "expected 411, got: {response:?}");
}

/// A request carrying `Expect: 100-continue` receives the interim `100
/// Continue` before the final response.
#[test]
fn expect_continue_gets_100_continue() {
    let chassis = boot_buffered::<EchoHttpHandler>(1024);
    let port = port_of(&chassis);

    // Poll the async `/` catch-all live before the interim-status assertion
    // (a `100 Continue` prefix precedes the dispatched status, so it cannot
    // itself be distinguished from a pre-registration `503` by prefix).
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let response = round_trip(
        port,
        b"POST /submit HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(response.starts_with("HTTP/1.1 100 Continue\r\n\r\n"), "expected interim 100 Continue, got: {response:?}");
    assert!(response.contains("HTTP/1.1 200 OK\r\n"), "expected final 200 after the interim, got: {response:?}");
}

/// A HEAD response carries the handler's headers — including the
/// `Content-Length` the body would have had — but no body bytes; a GET
/// to the same handler still returns the body.
#[test]
fn head_response_suppresses_body() {
    let chassis = boot_buffered::<FixedBodyHttpHandler>(1024);
    let port = port_of(&chassis);

    // First request against the async-registered `/` catch-all: poll it live.
    let head_response = round_trip_live(port, b"HEAD /x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(head_response.starts_with("HTTP/1.1 200 OK\r\n"), "expected 200, got: {head_response:?}");
    assert!(head_response.contains("Content-Length: 10\r\n"), "{head_response:?}");
    assert_eq!(body_of(&head_response), "", "HEAD must not carry a message body");

    let get_response = round_trip(port, b"GET /x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(body_of(&get_response), "fixed body");
}
