//! The HTTP edge's admission verdicts.
//!
//! These are the only failures answered with a status code rather than a
//! JSON-RPC body, so each one is a decision about what a *client* is told before
//! any protocol state exists. Getting one wrong is invisible from inside the
//! server and obvious from outside it: a `200` where a `401` belongs serves an
//! unauthenticated caller, and a `405` where a `202` belongs makes a
//! well-behaved client think the endpoint is broken.

use aether_http::kinds::{HttpHeader, HttpMethod, HttpServerRequest};

use crate::McpServerConfiguration;
use crate::protocol::PROTOCOL_REVISION;
use crate::runtime::transport::{admit, check_protocol_version};

const TOKEN: &str = "s3cret-token";

fn configured() -> McpServerConfiguration {
    McpServerConfiguration { enabled: true, authorization_token: TOKEN.to_string(), ..Default::default() }
}

/// A request that clears every check, so each test can spoil exactly one thing.
fn well_formed_post() -> HttpServerRequest {
    HttpServerRequest {
        method: HttpMethod::Post,
        path: "/mcp".to_string(),
        query: String::new(),
        headers: vec![
            header("authorization", &format!("Bearer {TOKEN}")),
            header("content-type", "application/json"),
            header("accept", "application/json, text/event-stream"),
        ],
        body: br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_vec(),
        peer_addr: "127.0.0.1:54321".to_string(),
    }
}

fn header(name: &str, value: &str) -> HttpHeader {
    HttpHeader { name: name.to_string(), value: value.to_string() }
}

fn with(mutate: impl FnOnce(&mut HttpServerRequest)) -> HttpServerRequest {
    let mut request = well_formed_post();
    mutate(&mut request);
    request
}

/// Replace a header the well-formed request already carries.
fn replace(request: &mut HttpServerRequest, name: &str, value: &str) {
    request.headers.retain(|held| !held.name.eq_ignore_ascii_case(name));
    request.headers.push(header(name, value));
}

fn status_of(request: &HttpServerRequest, config: &McpServerConfiguration) -> u16 {
    admit(request, config).err().map_or(200, |refusal| refusal.status)
}

/// The baseline the rest of the file spoils one field at a time. Without it a
/// test asserting `401` could be passing because of an unrelated header
/// mistake in the fixture rather than because of the rule it names.
#[test]
fn a_well_formed_post_is_admitted() {
    let request = well_formed_post();

    let admitted = admit(&request, &configured()).expect("the well-formed fixture must be admitted");

    assert_eq!(admitted.body, request.body, "the admitted body must be the request's own");
    assert_eq!(admitted.protocol_version, None, "the fixture declares no version header");
}

/// The transport lets a server decline the optional event stream with `405`,
/// and this server never opens one. A `GET` answered any other way would make a
/// client wait on a stream that will never arrive.
#[test]
fn get_and_delete_are_refused_as_method_not_allowed() {
    for method in [HttpMethod::Get, HttpMethod::Delete] {
        let request = with(|request| request.method = method);

        let refusal = admit(&request, &configured()).expect_err("only POST is served");

        assert_eq!(refusal.status, 405, "{method:?} must be method-not-allowed");
        assert!(
            refusal.headers.iter().any(|header| header.name.eq_ignore_ascii_case("allow")),
            "a 405 must name what is allowed: {:?}",
            refusal.headers,
        );
    }
}

/// The initialize result never mints a session identifier, so one can only have
/// been fabricated or copied from a different endpoint. Ignoring it would let a
/// client believe it holds session state this server has never had.
#[test]
fn a_fabricated_session_header_is_refused_loudly() {
    let request = with(|request| request.headers.push(header("mcp-session-id", "abc123")));

    let refusal = admit(&request, &configured()).expect_err("this server holds no session");

    assert_eq!(refusal.status, 400);
}

/// Every branch of the bearer guard, including the one that matters most: an
/// enabled server with no configured token refuses everything rather than
/// serving anyone who finds the port.
#[test]
fn the_bearer_guard_refuses_every_way_of_getting_it_wrong() {
    let unauthenticated = with(|request| request.headers.retain(|held| held.name != "authorization"));
    let wrong = with(|request| replace(request, "authorization", "Bearer wrong-token"));
    let truncated = with(|request| replace(request, "authorization", &format!("Bearer {}", &TOKEN[..4])));
    let wrong_scheme = with(|request| replace(request, "authorization", &format!("Basic {TOKEN}")));

    for request in [&unauthenticated, &wrong, &truncated, &wrong_scheme] {
        let refusal = admit(request, &configured()).expect_err("a bad token must be refused");
        assert_eq!(refusal.status, 401, "headers were {:?}", request.headers);
        assert!(
            refusal.headers.iter().any(|header| header.name.eq_ignore_ascii_case("www-authenticate")),
            "a 401 must carry the challenge",
        );
        assert!(!refusal.message.contains(TOKEN), "the refusal must not echo token material");
    }

    let unconfigured = McpServerConfiguration { enabled: true, ..Default::default() };
    assert_eq!(
        status_of(&well_formed_post(), &unconfigured),
        401,
        "an enabled server with no configured token must fail closed",
    );
}

/// An absent `Origin` is a native client and is accepted; a present one must be
/// on the allowlist, and the default empty allowlist therefore rejects every
/// browser. Conflating the two would either lock out every native caller or
/// hand a browser the DNS-rebinding hole the rule exists to close.
#[test]
fn origin_is_checked_only_when_present() {
    let browser = with(|request| request.headers.push(header("origin", "https://example.test")));

    assert_eq!(status_of(&browser, &configured()), 403, "the default allowlist admits no origin");
    assert_eq!(status_of(&well_formed_post(), &configured()), 200, "an absent origin is a native client");

    let mut permissive = configured();
    permissive.allowed_origins.insert("https://example.test".to_string());
    assert_eq!(status_of(&browser, &permissive), 200, "an allowlisted origin is admitted");
}

/// Both required media ranges must appear explicitly. A wildcard is the case
/// worth pinning: accepting it would mean a client that declared nothing gets
/// served as if it had declared both.
#[test]
fn accept_must_list_both_required_ranges_explicitly() {
    let json_only = with(|request| replace(request, "accept", "application/json"));
    let wildcard = with(|request| replace(request, "accept", "*/*"));
    let zero_quality = with(|request| replace(request, "accept", "application/json, text/event-stream;q=0"));
    let absent = with(|request| request.headers.retain(|held| held.name != "accept"));

    for request in [&json_only, &wildcard, &zero_quality, &absent] {
        assert_eq!(status_of(request, &configured()), 406, "headers were {:?}", request.headers);
    }

    let with_parameters = with(|request| replace(request, "accept", "text/event-stream;q=0.9, application/json;q=1.0"));
    assert_eq!(status_of(&with_parameters, &configured()), 200, "positive quality on both is acceptable");
}

/// The content type admits `application/json` with an optional UTF-8 charset
/// and nothing else, and no content encoding other than identity — a compressed
/// body would be handed to the JSON parser as bytes it cannot read.
#[test]
fn the_content_type_and_encoding_are_narrow() {
    let wrong_media = with(|request| replace(request, "content-type", "text/plain"));
    let wrong_charset = with(|request| replace(request, "content-type", "application/json; charset=iso-8859-1"));
    let absent = with(|request| request.headers.retain(|held| held.name != "content-type"));
    let compressed = with(|request| request.headers.push(header("content-encoding", "gzip")));

    for request in [&wrong_media, &wrong_charset, &absent, &compressed] {
        assert_eq!(status_of(request, &configured()), 415, "headers were {:?}", request.headers);
    }

    let charset = with(|request| replace(request, "content-type", "Application/JSON; charset=UTF-8"));
    assert_eq!(status_of(&charset, &configured()), 200, "media type and parameters compare case-insensitively");
}

/// The version header is mandatory *after* initialize and absent *on* it.
///
/// The absent-after-initialize case is the subtle one: the transport says a
/// server with no retained negotiated state must then assume the previous
/// revision, which this server does not implement, so answering under
/// 2025-06-18 anyway would serve a contract neither side agreed to.
#[test]
fn the_protocol_version_header_is_required_after_initialize() {
    assert!(check_protocol_version(None, true).is_ok(), "initialize negotiates the version, so it declares none");
    assert_eq!(check_protocol_version(None, false).expect_err("a later request must declare its revision").status, 400,);
    assert_eq!(check_protocol_version(Some("2025-03-26"), false).expect_err("another revision is refused").status, 400,);
    assert_eq!(
        check_protocol_version(Some("2025-03-26"), true).expect_err("even initialize must not claim another").status,
        400,
    );
    assert!(check_protocol_version(Some(PROTOCOL_REVISION), false).is_ok());
}
