use super::{
    HttpResponseStreamOpen, OPCODE_BINARY, OPCODE_CONTINUATION, OPCODE_TEXT, WsFrameParse,
    http_date, normalize_prefix, parse_http_method, parse_ws_frame, percent_decode_path,
    reason_phrase, render_stream_head, request_keeps_alive, route_matches, sec_websocket_accept,
    serialize_ws_frame, sha1, validate_ws_handshake,
};
use crate::http::kinds::{HttpHeader, HttpMethod};
use std::time::{Duration, UNIX_EPOCH};

fn conn_header(value: &str) -> Vec<HttpHeader> {
    vec![HttpHeader {
        name: "Connection".to_string(),
        value: value.to_string(),
    }]
}

/// Tripwire: keep-alive defaulting is branch logic over the HTTP version
/// and the `Connection` header, not a derived mirror — HTTP/1.1 keeps
/// alive unless told to close, HTTP/1.0 closes unless told to keep alive,
/// and an explicit token wins over the version default either way.
#[test]
fn keep_alive_defaults_by_version_and_connection_header() {
    // HTTP/1.1 (version 1): keep-alive by default, `close` overrides.
    assert!(request_keeps_alive(Some(1), &[]));
    assert!(!request_keeps_alive(Some(1), &conn_header("close")));
    assert!(request_keeps_alive(Some(1), &conn_header("keep-alive")));
    // HTTP/1.0 (version 0): close by default, `keep-alive` overrides.
    assert!(!request_keeps_alive(Some(0), &[]));
    assert!(request_keeps_alive(Some(0), &conn_header("keep-alive")));
    assert!(!request_keeps_alive(Some(0), &conn_header("close")));
    // Case-insensitive, and a token among comma-separated values counts.
    assert!(!request_keeps_alive(Some(1), &conn_header("Close")));
    assert!(request_keeps_alive(
        Some(0),
        &conn_header("keep-alive, Upgrade")
    ));
}

/// Segment-boundary semantics (ADR-0130): a prefix matches at `/`
/// boundaries only, so `/api` never captures `/apiary`.
#[test]
fn route_match_is_segment_boundary() {
    assert!(route_matches("/api", "/api"));
    assert!(route_matches("/api", "/api/widgets"));
    assert!(!route_matches("/api", "/apiary"));
    assert!(!route_matches("/api", "/ap"));
    assert!(route_matches("/", "/anything"));
    assert!(route_matches("/", "/"));
}

/// Prefix normalization: leading `/` required, trailing slashes
/// stripped to one canonical spelling, `/` kept as the catch-all.
#[test]
fn prefix_normalization() {
    assert_eq!(normalize_prefix("/api/"), Ok("/api".to_string()));
    assert_eq!(normalize_prefix("/api"), Ok("/api".to_string()));
    assert_eq!(normalize_prefix("/"), Ok("/".to_string()));
    assert_eq!(normalize_prefix("///"), Ok("/".to_string()));
    assert!(normalize_prefix("api").is_err());
    assert!(normalize_prefix("").is_err());
}

#[test]
fn http_date_formats_the_rfc_example() {
    // RFC 7231 §7.1.1.1 canonical example.
    let when = UNIX_EPOCH + Duration::from_secs(784_111_777);
    assert_eq!(http_date(when), "Sun, 06 Nov 1994 08:49:37 GMT");
}

#[test]
fn known_methods_map_unknown_is_none() {
    assert_eq!(parse_http_method("GET"), Some(HttpMethod::Get));
    assert_eq!(parse_http_method("POST"), Some(HttpMethod::Post));
    assert_eq!(parse_http_method("OPTIONS"), Some(HttpMethod::Options));
    assert_eq!(parse_http_method("FROB"), None);
    assert_eq!(parse_http_method("get"), None);
}

/// Tripwire: `render_stream_head`'s `Connection` disposition is branch logic
/// over its `keep_alive` argument, not a hardcoded `close` — pins the ADR-0128
/// keep-alive-after-stream fix (issue #2582) at the unit level.
#[test]
fn render_stream_head_honors_keep_alive() {
    let open = HttpResponseStreamOpen {
        status: 200,
        headers: vec![],
    };
    let keep_alive_head = String::from_utf8(render_stream_head(&open, true)).expect("head is utf8");
    assert!(keep_alive_head.contains("Connection: keep-alive\r\n"));
    assert!(!keep_alive_head.contains("Connection: close"));

    let close_head = String::from_utf8(render_stream_head(&open, false)).expect("head is utf8");
    assert!(close_head.contains("Connection: close\r\n"));
    assert!(!close_head.contains("Connection: keep-alive"));
}

#[test]
fn reason_phrases_cover_emitted_statuses() {
    assert_eq!(reason_phrase(200), "OK");
    assert_eq!(reason_phrase(411), "Length Required");
    assert_eq!(reason_phrase(413), "Payload Too Large");
    assert_eq!(reason_phrase(501), "Not Implemented");
    assert_eq!(reason_phrase(502), "Bad Gateway");
    assert_eq!(reason_phrase(503), "Service Unavailable");
    assert_eq!(reason_phrase(504), "Gateway Timeout");
}

#[test]
fn percent_decode_path_decodes_valid_escapes_and_passes_through_invalid_ones() {
    assert_eq!(percent_decode_path("/hello%20world"), "/hello world");
    assert_eq!(percent_decode_path("/no-escapes"), "/no-escapes");
    // Trailing `%` / `%2` (too short for a full escape) pass through
    // literally rather than erroring.
    assert_eq!(percent_decode_path("/trailing%"), "/trailing%");
    assert_eq!(percent_decode_path("/trailing%2"), "/trailing%2");
    // Non-hex digits pass through literally.
    assert_eq!(percent_decode_path("/bad%zzescape"), "/bad%zzescape");
}

#[test]
fn sha1_matches_the_rfc_3174_vector() {
    // Tripwire: RFC 3174 §7.3 worked example sha1("abc"). A computed digest
    // that drifts if the block schedule / padding / round logic breaks.
    use std::fmt::Write as _;
    let digest = sha1(b"abc");
    let hex = digest.iter().fold(String::new(), |mut acc, byte| {
        let _ = write!(acc, "{byte:02x}");
        acc
    });
    assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
}

#[test]
fn sec_websocket_accept_matches_the_rfc_6455_vector() {
    // Tripwire: RFC 6455 §1.3 worked handshake vector — base64(SHA-1(key +
    // GUID)). A computed value pinning the SHA-1, the GUID, and the base64
    // together; it drifts if any of the three is wrong (the GUID's last
    // byte especially).
    assert_eq!(
        sec_websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}

#[test]
fn ws_frame_serialize_matches_rfc_6455_examples() {
    // Tripwire: RFC 6455 §5.7 byte-layout examples — computed frame bytes.
    // #1 single unmasked text "Hello".
    assert_eq!(
        serialize_ws_frame(OPCODE_TEXT, b"Hello", None),
        vec![0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f]
    );
    // #2 single masked text "Hello" (mask 0x37fa213d).
    assert_eq!(
        serialize_ws_frame(OPCODE_TEXT, b"Hello", Some([0x37, 0xfa, 0x21, 0x3d])),
        vec![
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58
        ]
    );
    // #4 256-byte binary: the 16-bit extended-length header.
    let big = serialize_ws_frame(OPCODE_BINARY, &[0u8; 256], None);
    assert_eq!(&big[..4], &[0x82, 0x7e, 0x01, 0x00]);
    // #5 65536-byte binary: the 64-bit extended-length header.
    let huge = serialize_ws_frame(OPCODE_BINARY, &vec![0u8; 65_536], None);
    assert_eq!(
        &huge[..10],
        &[0x82, 0x7f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]
    );
}

#[test]
fn ws_frame_parse_unmasks_the_rfc_6455_masked_example() {
    // Tripwire: parse RFC 6455 §5.7 #2 (masked "Hello") back to its
    // payload — the mask XOR and header decode as a round-trip of the
    // serialize tripwire above.
    let bytes = [
        0x81u8, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
    ];
    match parse_ws_frame(&bytes, 1024) {
        WsFrameParse::Complete { frame, consumed } => {
            assert!(frame.fin);
            assert_eq!(frame.opcode, OPCODE_TEXT);
            assert_eq!(frame.payload, b"Hello");
            assert_eq!(consumed, bytes.len());
        }
        _ => panic!("expected a complete frame"),
    }
}

#[test]
fn ws_frame_parse_rejects_an_unmasked_client_frame() {
    // A client→server frame MUST be masked (RFC 6455 §5.1); an unmasked one
    // is a `1002` protocol error, never a panic on untrusted input.
    let bytes = [0x81u8, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
    assert!(matches!(
        parse_ws_frame(&bytes, 1024),
        WsFrameParse::Error { code: 1002, .. }
    ));
}

#[test]
fn ws_frame_parse_needs_more_on_a_partial_frame() {
    // A header that announces more payload than is buffered yields NeedMore,
    // not a panic or a wrong-length read.
    let bytes = [0x81u8, 0x85, 0x37, 0xfa, 0x21]; // header cut mid-mask-key
    assert!(matches!(
        parse_ws_frame(&bytes, 1024),
        WsFrameParse::NeedMore
    ));
}

#[test]
fn ws_continuation_frames_decode_as_a_fragmented_message() {
    // Tripwire: a fragmented text message "Hel" + "lo" as masked frames —
    // the first non-final (FIN clear, opcode text), the second final (FIN
    // set, opcode continuation). Field-by-field decode of masking + the FIN
    // bit + the continuation opcode; concatenating the payloads yields the
    // original message (the reassembly the frame loop performs).
    let mask = [0x01u8, 0x02, 0x03, 0x04];
    let mut first = serialize_ws_frame(OPCODE_TEXT, b"Hel", Some(mask));
    first[0] &= 0x7f; // clear FIN — a non-final fragment
    let second = serialize_ws_frame(OPCODE_CONTINUATION, b"lo", Some(mask));

    let WsFrameParse::Complete { frame: f1, .. } = parse_ws_frame(&first, 1024) else {
        panic!("first fragment must parse");
    };
    assert!(!f1.fin);
    assert_eq!(f1.opcode, OPCODE_TEXT);
    assert_eq!(f1.payload, b"Hel");

    let WsFrameParse::Complete { frame: f2, .. } = parse_ws_frame(&second, 1024) else {
        panic!("continuation fragment must parse");
    };
    assert!(f2.fin);
    assert_eq!(f2.opcode, OPCODE_CONTINUATION);
    assert_eq!(f2.payload, b"lo");

    let mut message = f1.payload;
    message.extend_from_slice(&f2.payload);
    assert_eq!(message, b"Hello");
}

#[test]
fn ws_handshake_validation_enforces_version_and_key() {
    let base = |extra: &[(&str, &str)]| -> Vec<HttpHeader> {
        let mut headers = vec![HttpHeader {
            name: "Connection".to_string(),
            value: "Upgrade".to_string(),
        }];
        for (name, value) in extra {
            headers.push(HttpHeader {
                name: (*name).to_string(),
                value: (*value).to_string(),
            });
        }
        headers
    };
    // Valid: version 13 + a key echoes the key back.
    assert_eq!(
        validate_ws_handshake(&base(&[
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Key", "abc"),
        ])),
        Ok("abc".to_string())
    );
    // Wrong version → 426.
    assert!(matches!(
        validate_ws_handshake(&base(&[
            ("Sec-WebSocket-Version", "8"),
            ("Sec-WebSocket-Key", "abc"),
        ])),
        Err((426, _))
    ));
    // Missing key → 400.
    assert!(matches!(
        validate_ws_handshake(&base(&[("Sec-WebSocket-Version", "13")])),
        Err((400, _))
    ));
    // Missing Connection: Upgrade → 400.
    assert!(matches!(
        validate_ws_handshake(&[
            HttpHeader {
                name: "Sec-WebSocket-Version".to_string(),
                value: "13".to_string(),
            },
            HttpHeader {
                name: "Sec-WebSocket-Key".to_string(),
                value: "abc".to_string(),
            },
        ]),
        Err((400, _))
    ));
}

#[test]
fn config_layer_defaults_match_the_named_consts() {
    use super::super::{
        DEFAULT_BIND_ADDR, DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS, DEFAULT_MAX_CONNECTIONS,
        DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_REQUEST_BYTES, DEFAULT_REQUEST_STREAM_WINDOW,
        DEFAULT_REQUEST_TIMEOUT_MILLIS, DEFAULT_RESPONSE_STREAM_WINDOW,
        DEFAULT_WS_IDLE_TIMEOUT_MILLIS, HttpServerConfig, HttpServerConfigLayer,
    };
    use confique::Config as _;
    // No `.env()` source: loads the literal defaults only, so this is
    // env-free and guards the layer defaults against the consts +
    // `HttpServerConfig::default()`.
    let layer = HttpServerConfigLayer::builder()
        .load()
        .expect("defaults load");
    let default = HttpServerConfig::default();
    assert_eq!(layer.bind_addr, DEFAULT_BIND_ADDR);
    assert_eq!(layer.bind_addr, default.bind_addr);
    assert_eq!(layer.handler_mailbox, "");
    assert_eq!(layer.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
    assert_eq!(layer.max_header_bytes, DEFAULT_MAX_HEADER_BYTES);
    assert_eq!(layer.request_timeout_millis, DEFAULT_REQUEST_TIMEOUT_MILLIS);
    assert_eq!(
        layer.keep_alive_timeout_millis,
        DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS
    );
    assert_eq!(
        default.keep_alive_timeout_millis,
        DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS
    );
    assert_eq!(layer.max_connections, DEFAULT_MAX_CONNECTIONS);
    assert_eq!(layer.max_connections, default.max_connections);
    assert_eq!(layer.response_stream_window, DEFAULT_RESPONSE_STREAM_WINDOW);
    assert_eq!(
        default.response_stream_window,
        DEFAULT_RESPONSE_STREAM_WINDOW
    );
    assert_eq!(layer.request_stream_window, DEFAULT_REQUEST_STREAM_WINDOW);
    assert_eq!(default.request_stream_window, DEFAULT_REQUEST_STREAM_WINDOW);
    assert_eq!(
        layer.websocket_idle_timeout_millis,
        DEFAULT_WS_IDLE_TIMEOUT_MILLIS
    );
    assert_eq!(
        default.websocket_idle_timeout_millis,
        DEFAULT_WS_IDLE_TIMEOUT_MILLIS
    );
}
