//! Route registration and selection (ADR-0130 / ADR-0131 / ADR-0154): a
//! `wire`-registered prefix dispatching as its minted kind, typed extractors,
//! path templates, deferred routes, longest-prefix and method precedence,
//! mid-connection registration, macro/hand-written composition, and
//! self-unregistration.

use aether_actor::Addressable;
use aether_data::Kind as KindTrait;
use aether_data::KindId;
use aether_substrate::Mail;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::testing::{TestChassis, fresh_substrate};
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::kinds::{HttpServerRequest as RequestKind, RegisterRoute};
use crate::server::HttpServerCapability;

use super::handlers::{
    ApiRouteHandler, ApiV2Handler, BookRouteHandler, DeferRouteHandler, EchoHttpHandler, EchoPeer, ExtractRouteHandler,
    FixedBodyHttpHandler, MethodAnyHandler, MethodPostHandler, SilentPeer, TmpRouteHandler, WiredRouteHandler,
};
use super::support::{
    body_of, config_for, keep_alive_config_for, poll_body, port_of, read_one_response, round_trip, round_trip_live,
};

/// Boot the server (first, so the routed handlers' `wire` registrations
/// find its mailbox live) with [`FixedBodyHttpHandler`] as the `/`
/// catch-all (its `wire` binds `prefix: "/"`), then the given routed
/// handlers.
macro_rules! routed_chassis {
    ($($handler:ty),+ $(,)?) => {{
        let (registry, mailer) = fresh_substrate();
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor_configured::<HttpServerCapability>((), config_for(1024))
            .with_actor::<FixedBodyHttpHandler>(())
            $(.with_actor::<$handler>(()))+
            .build_passive()
            .expect("caps boot")
    }};
}

/// A `wire`-registered route dispatches as the registered kind — the
/// handler's typed `#[handler]` decodes the request-shaped payload under
/// the minted kind and echoes the path — a deeper path under the claimed
/// prefix is not swallowed but 404s (#3697), and an unrouted path falls
/// back to the `/` catch-all route (ADR-0130).
#[test]
fn routed_prefix_dispatches_as_registered_kind() {
    let chassis = routed_chassis!(ApiRouteHandler);
    let port = port_of(&chassis);

    poll_body(port, b"GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n", "api:/api");

    // A deeper path under the claimed prefix is not swallowed (#3697): the cap
    // routes it to the `/api` dispatcher, which matches exactly and 404s rather
    // than absorbing the deeper path.
    let deeper = round_trip(port, b"GET /api/widgets HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(deeper.starts_with("HTTP/1.1 404 "), "an exact route does not swallow a deeper path: {deeper:?}");

    // A path under no claimed prefix falls back to the `/` catch-all — its own
    // async registration, so poll it live.
    poll_body(port, b"GET /other HTTP/1.1\r\nHost: localhost\r\n\r\n", "fixed body");
}

/// A macro-authored route with a real `FromRequest` extractor dispatches
/// the routed method with the extracted value on success, and — when the
/// extractor returns `Err` — replies that response without ever calling
/// the handler (ADR-0131's typed `400` boundary), both over the wire.
#[test]
fn routed_extractor_success_and_failure() {
    let chassis = routed_chassis!(ExtractRouteHandler);
    let port = port_of(&chassis);

    // Success: the extracted `name` reaches the handler and is echoed.
    poll_body(port, b"GET /extract?name=ada HTTP/1.1\r\nHost: localhost\r\n\r\n", "hello:ada");

    // Failure: with the route proven live, a request missing `name`
    // short-circuits to the extractor's 400 response body.
    let missing = round_trip(port, b"GET /extract HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(missing.starts_with("HTTP/1.1 400 "), "extractor Err becomes the reply status: {missing:?}");
    assert_eq!(body_of(&missing), "missing name query parameter", "extractor Err body is replied verbatim");
}

/// ADR-0154 path templates end to end: nested routes sharing the `/books`
/// static head dispatch by segment + method, `{id}` binds through
/// `Path<u64>`, a non-numeric id is the `FromPathSegment` `400`, and a
/// path the group has no exact template for is a `404`.
#[test]
fn path_template_routes_dispatch_and_capture() {
    let chassis = routed_chassis!(BookRouteHandler);
    let port = port_of(&chassis);

    // Collection and captured-member routes share one (Get, /books) group;
    // `/books` and `/books/{id}` each match their exact segment count, so a
    // member request selects the capture route.
    poll_body(port, b"GET /books HTTP/1.1\r\nHost: localhost\r\n\r\n", "books:list");
    poll_body(port, b"GET /books/42 HTTP/1.1\r\nHost: localhost\r\n\r\n", "books:get:42");

    // The sibling POST group under the same static head, with the capture.
    poll_body(port, b"POST /books/7/checkout HTTP/1.1\r\nHost: localhost\r\n\r\n", "books:checkout:7");

    // A non-numeric capture short-circuits to the FromPathSegment 400
    // rather than falling through to the `/books` prefix.
    let bad = round_trip(port, b"GET /books/notanumber HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(bad.starts_with("HTTP/1.1 400 "), "non-numeric id is a 400: {bad:?}");

    // The POST group has only the exact `/books/{id}/checkout` template and
    // no bare-prefix fallback, so a POST the group can't match is a 404.
    let miss = round_trip(port, b"POST /books/7 HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(miss.starts_with("HTTP/1.1 404 "), "no exact template match is a 404: {miss:?}");
}

/// ADR-0154 §2 deferred routes end to end (relay pattern): `GET /echo`
/// forwards its request to a peer by type via `defer::<EchoPeer>` — an
/// inherited `send_with_context` that keeps the request's chain open — and
/// the reply route answers on the peer's `EchoSay`. `GET /blackhole` forwards
/// to a peer that settles without replying, so the request's chain settles
/// response-less and the server's own `502` net answers.
#[test]
fn deferred_route_forwards_and_answers_on_reply() {
    let chassis = routed_chassis!(DeferRouteHandler, EchoPeer, SilentPeer);
    let port = port_of(&chassis);

    // The reply arrives from the peer and the reply route answers the held
    // request; poll it live past the async route registration.
    poll_body(port, b"GET /echo HTTP/1.1\r\nHost: localhost\r\n\r\n", "echoed:hi");

    // The silent peer settles its inbound without replying, so the request's
    // chain settles response-less and the server answers `502`. Before the
    // `/blackhole` registration lands the path takes the `/` catch-all (a
    // 200), so poll until the 502 appears.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = round_trip(port, b"GET /blackhole HTTP/1.1\r\nHost: localhost\r\n\r\n");
        if response.starts_with("HTTP/1.1 502 ") {
            break;
        }
        assert!(Instant::now() < deadline, "expected a 502 within 10s; last response: {response:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Longest registered prefix wins among overlapping routes (an exact
/// `/api/v2` request beats the `/api` route), matching stops at segment
/// boundaries (`/apiary` is not under `/api`), and a deeper path under the
/// winning prefix is not swallowed — it 404s at that dispatcher (#3697,
/// ADR-0130).
#[test]
fn longest_prefix_wins_on_segment_boundaries() {
    let chassis = routed_chassis!(ApiRouteHandler, ApiV2Handler);
    let port = port_of(&chassis);

    // The exact `/api` hit routes to the `/api` handler.
    poll_body(port, b"GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n", "api:/api");

    // Longest registered prefix wins: `/api/v2` beats `/api` for an exact
    // `/api/v2` request (had `/api` won, its exact route would not match the
    // deeper path and would 404 — so `api-v2` proves `/api/v2` was selected).
    poll_body(port, b"GET /api/v2 HTTP/1.1\r\nHost: localhost\r\n\r\n", "api-v2");

    // A deeper path under the winning prefix is not swallowed (#3697): it
    // routes to the `/api/v2` dispatcher, which matches exactly and 404s.
    let deeper = round_trip(port, b"GET /api/v2/x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(deeper.starts_with("HTTP/1.1 404 "), "exact route does not swallow a deeper path: {deeper:?}");

    // `/apiary` is not under `/api` (segment boundary), so it takes the `/`
    // catch-all; poll it live (its own async registration) before asserting.
    poll_body(port, b"GET /apiary HTTP/1.1\r\nHost: localhost\r\n\r\n", "fixed body");
}

/// A method-specific route beats a method-agnostic one at equal
/// prefix; other methods take the agnostic route (ADR-0130).
#[test]
fn method_specific_route_beats_agnostic() {
    let chassis = routed_chassis!(MethodPostHandler, MethodAnyHandler);
    let port = port_of(&chassis);

    // Each route's first positive assertion polls it live; together
    // they then pin the precedence (POST → specific, GET → agnostic).
    poll_body(port, b"POST /m HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n", "post-m");
    poll_body(port, b"GET /m HTTP/1.1\r\nHost: localhost\r\n\r\n", "any-m");

    let specific = round_trip(port, b"POST /m HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(body_of(&specific), "post-m", "POST takes the method-specific route with both routes live");
}

/// A route registered mid-connection is visible to the very next
/// request on an already-kept-alive socket (ADR-0135 §2): the reader
/// re-reads the shared route table per request head, so registration
/// granularity is next-request, not next-connection.
///
/// Tripwire: a reader-side route *snapshot* taken at connection
/// adoption would serve the catch-all forever on a long-lived
/// connection; this test's second-phase request would never flip to
/// the routed body.
///
/// The `/late` target is [`WiredRouteHandler`] (its generic `on_extra`
/// serves `HttpServerRequest` and its `wire` claims only `/wired…`,
/// never `/`), so the sole `/` catch-all here is [`EchoHttpHandler`] —
/// two handlers both claiming `/` would be a registration conflict.
#[test]
fn route_registered_mid_connection_serves_next_request() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<WiredRouteHandler>(())
        .with_actor_configured::<HttpServerCapability>((), keep_alive_config_for(5_000))
        .build_passive()
        .expect("caps boot");
    let port = port_of(&chassis);

    // Poll the echo `/` catch-all live before the pre-registration read.
    round_trip_live(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("set_read_timeout");
    let mut carry = Vec::new();

    // Pre-registration: /late takes the echo `/` catch-all.
    stream.write_all(b"GET /late HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write request");
    let first = read_one_response(&mut stream, &mut carry);
    assert!(first.contains("x-aether-path: /late"), "pre-registration request takes the echo catch-all: {first:?}");

    // Register /late at the wired handler while the connection is
    // parked between keep-alive requests.
    let supervisor = registry.lookup(<HttpServerCapability as Addressable>::NAMESPACE).expect("http server registered");
    let target = registry.lookup(<WiredRouteHandler as Addressable>::NAMESPACE).expect("wired handler registered");
    let payload = RegisterRoute {
        prefix: "/late".to_string(),
        method: None,
        kind: <RequestKind as KindTrait>::ID,
        mailbox: target,
        shared: false,
    }
    .encode_into_bytes();
    mailer.push(Mail::new(supervisor, KindId(<RegisterRoute as KindTrait>::ID.0), payload, 1));

    // The registration lands asynchronously; poll on the SAME socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        stream.write_all(b"GET /late HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write request");
        let response = read_one_response(&mut stream, &mut carry);
        if body_of(&response) == "wired-raw" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mid-connection registration should reach the next request within 10s; \
             last: {response:?}",
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// A macro route composes with a hand-written `wire`: the macro appends
/// its `/wired` registration to the author's `wire` without displacing
/// the raw `/wired-extra` claim already there, so both dispatch (ADR-0131
/// append path).
#[test]
fn hand_written_wire_and_macro_route_compose() {
    let chassis = routed_chassis!(WiredRouteHandler);
    let port = port_of(&chassis);

    // The macro-appended registration reaches the cap.
    poll_body(port, b"GET /wired HTTP/1.1\r\nHost: localhost\r\n\r\n", "wired-macro");
    // The author's own `wire` registration survived the append.
    poll_body(port, b"GET /wired-extra HTTP/1.1\r\nHost: localhost\r\n\r\n", "wired-raw");
}

/// `unregister_route_self` releases the sender's route: the first
/// request reaches the routed handler (which releases the route while
/// answering), and a subsequent request falls back. The release is a
/// separate mail racing the reply, so the fallback is asserted with a
/// bounded poll rather than a single follow-up request.
#[test]
fn self_unregister_releases_route() {
    let chassis = routed_chassis!(TmpRouteHandler);
    let port = port_of(&chassis);

    // Poll the route live (a pre-registration request harmlessly falls
    // back without triggering the handler's release); the first "tmp"
    // response is also the one that releases the route.
    poll_body(port, b"GET /tmp HTTP/1.1\r\nHost: localhost\r\n\r\n", "tmp");

    // The release is a separate mail racing the reply, so the fallback
    // is asserted with the same bounded poll.
    poll_body(port, b"GET /tmp HTTP/1.1\r\nHost: localhost\r\n\r\n", "fixed body");
}
