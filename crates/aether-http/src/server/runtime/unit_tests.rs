use super::{
    Arc, HttpResponseStreamOpen, KindId, MailboxId, OPCODE_BINARY, OPCODE_CONTINUATION, OPCODE_TEXT,
    RegisterRouteResult, RwLock, SharedRoutes, WsFrameParse, http_date, normalize_prefix, parse_http_method,
    parse_ws_frame, percent_decode_path, reason_phrase, register_route, render_stream_head, request_keeps_alive,
    route_matches, sec_websocket_accept, serialize_ws_frame, sha1, unregister_route, unregister_routes_all,
    validate_ws_handshake,
};
use crate::kinds::{HttpHeader, HttpMethod};
use std::time::{Duration, UNIX_EPOCH};

fn conn_header(value: &str) -> Vec<HttpHeader> {
    vec![HttpHeader { name: "Connection".to_string(), value: value.to_string() }]
}

/// ADR-0155 §3: a server composed disabled claims its mailbox but binds
/// no socket, so its route-registration surface must fail fast with an
/// `Err` reply (the fail-fast convention the headless caps use) rather
/// than the mail warn-dropping at an unknown mailbox. The disabled branch
/// returns before touching the registry, so a bare ctx suffices.
#[test]
fn disabled_http_server_err_replies_to_register_route() {
    use super::{HttpServerCapability, HttpServerConfig, HttpSupervisorState, NativeCtx};
    use crate::kinds::RegisterRoute;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::{MailId, Source};
    use aether_substrate::testing::fresh_substrate;

    let (_registry, mailer) = fresh_substrate();
    let mut state = HttpSupervisorState::disabled(HttpServerConfig::default(), Arc::clone(&mailer));
    let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
    let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

    let result = HttpServerCapability::on_register_route(
        &mut state,
        &mut ctx,
        RegisterRoute { prefix: "/".to_string(), method: None, kind: KindId(0), mailbox: MailboxId(1), shared: false },
    );
    assert!(
        matches!(result, RegisterRouteResult::Err { .. }),
        "a disabled http server must fail fast on register_route, got {result:?}",
    );
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
    assert!(request_keeps_alive(Some(0), &conn_header("keep-alive, Upgrade")));
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
    let open = HttpResponseStreamOpen { status: 200, headers: vec![] };
    let keep_alive_head = String::from_utf8(render_stream_head(&open, true)).expect("head is utf8");
    assert!(keep_alive_head.contains("Connection: keep-alive\r\n"));
    assert!(!keep_alive_head.contains("Connection: close"));

    let close_head = String::from_utf8(render_stream_head(&open, false)).expect("head is utf8");
    assert!(close_head.contains("Connection: close\r\n"));
    assert!(!close_head.contains("Connection: keep-alive"));
}

/// Tripwire: `as_str` (render) and `parse_http_method` (parse) are two
/// independent match tables owning the same seven canonical spellings; this
/// pins that they never drift apart across every variant.
#[test]
fn http_method_as_str_round_trips_through_parse_http_method() {
    for method in [
        HttpMethod::Get,
        HttpMethod::Post,
        HttpMethod::Put,
        HttpMethod::Delete,
        HttpMethod::Patch,
        HttpMethod::Head,
        HttpMethod::Options,
    ] {
        assert_eq!(parse_http_method(method.as_str()), Some(method));
    }
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
    assert_eq!(sec_websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

#[test]
fn ws_frame_serialize_matches_rfc_6455_examples() {
    // Tripwire: RFC 6455 §5.7 byte-layout examples — computed frame bytes.
    // #1 single unmasked text "Hello".
    assert_eq!(serialize_ws_frame(OPCODE_TEXT, b"Hello", None), vec![0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f]);
    // #2 single masked text "Hello" (mask 0x37fa213d).
    assert_eq!(
        serialize_ws_frame(OPCODE_TEXT, b"Hello", Some([0x37, 0xfa, 0x21, 0x3d])),
        vec![0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58]
    );
    // #4 256-byte binary: the 16-bit extended-length header.
    let big = serialize_ws_frame(OPCODE_BINARY, &[0u8; 256], None);
    assert_eq!(&big[..4], &[0x82, 0x7e, 0x01, 0x00]);
    // #5 65536-byte binary: the 64-bit extended-length header.
    let huge = serialize_ws_frame(OPCODE_BINARY, &vec![0u8; 65_536], None);
    assert_eq!(&huge[..10], &[0x82, 0x7f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]);
}

#[test]
fn ws_frame_parse_unmasks_the_rfc_6455_masked_example() {
    // Tripwire: parse RFC 6455 §5.7 #2 (masked "Hello") back to its
    // payload — the mask XOR and header decode as a round-trip of the
    // serialize tripwire above.
    let bytes = [0x81u8, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
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
    assert!(matches!(parse_ws_frame(&bytes, 1024), WsFrameParse::Error { code: 1002, .. }));
}

#[test]
fn ws_frame_parse_needs_more_on_a_partial_frame() {
    // A header that announces more payload than is buffered yields NeedMore,
    // not a panic or a wrong-length read.
    let bytes = [0x81u8, 0x85, 0x37, 0xfa, 0x21]; // header cut mid-mask-key
    assert!(matches!(parse_ws_frame(&bytes, 1024), WsFrameParse::NeedMore));
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
        let mut headers = vec![HttpHeader { name: "Connection".to_string(), value: "Upgrade".to_string() }];
        for (name, value) in extra {
            headers.push(HttpHeader { name: (*name).to_string(), value: (*value).to_string() });
        }
        headers
    };
    // Valid: version 13 + a key echoes the key back.
    assert_eq!(
        validate_ws_handshake(&base(&[("Sec-WebSocket-Version", "13"), ("Sec-WebSocket-Key", "abc"),])),
        Ok("abc".to_string())
    );
    // Wrong version → 426.
    assert!(matches!(
        validate_ws_handshake(&base(&[("Sec-WebSocket-Version", "8"), ("Sec-WebSocket-Key", "abc"),])),
        Err((426, _))
    ));
    // Missing key → 400.
    assert!(matches!(validate_ws_handshake(&base(&[("Sec-WebSocket-Version", "13")])), Err((400, _))));
    // Missing Connection: Upgrade → 400.
    assert!(matches!(
        validate_ws_handshake(&[
            HttpHeader { name: "Sec-WebSocket-Version".to_string(), value: "13".to_string() },
            HttpHeader { name: "Sec-WebSocket-Key".to_string(), value: "abc".to_string() },
        ]),
        Err((400, _))
    ));
}

#[test]
fn config_layer_defaults_match_the_named_consts() {
    use super::super::{
        DEFAULT_BIND_ADDR, DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS, DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_HEADER_BYTES,
        DEFAULT_MAX_REQUEST_BYTES, DEFAULT_REQUEST_STREAM_WINDOW, DEFAULT_REQUEST_TIMEOUT_MILLIS,
        DEFAULT_RESPONSE_STREAM_WINDOW, DEFAULT_WS_IDLE_TIMEOUT_MILLIS, HttpServerConfig, HttpServerConfigLayer,
    };
    use confique::Config as _;
    // No `.env()` source: loads the literal defaults only, so this is
    // env-free and guards the layer defaults against the consts +
    // `HttpServerConfig::default()`.
    let layer = HttpServerConfigLayer::builder().load().expect("defaults load");
    let default = HttpServerConfig::default();
    assert_eq!(layer.bind_addr, DEFAULT_BIND_ADDR);
    assert_eq!(layer.bind_addr, default.bind_addr);
    assert_eq!(layer.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
    assert_eq!(layer.max_header_bytes, DEFAULT_MAX_HEADER_BYTES);
    assert_eq!(layer.request_timeout_millis, DEFAULT_REQUEST_TIMEOUT_MILLIS);
    assert_eq!(layer.keep_alive_timeout_millis, DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS);
    assert_eq!(default.keep_alive_timeout_millis, DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS);
    assert_eq!(layer.max_connections, DEFAULT_MAX_CONNECTIONS);
    assert_eq!(layer.max_connections, default.max_connections);
    assert_eq!(layer.response_stream_window, DEFAULT_RESPONSE_STREAM_WINDOW);
    assert_eq!(default.response_stream_window, DEFAULT_RESPONSE_STREAM_WINDOW);
    assert_eq!(layer.request_stream_window, DEFAULT_REQUEST_STREAM_WINDOW);
    assert_eq!(default.request_stream_window, DEFAULT_REQUEST_STREAM_WINDOW);
    assert_eq!(layer.websocket_idle_timeout_millis, DEFAULT_WS_IDLE_TIMEOUT_MILLIS);
    assert_eq!(default.websocket_idle_timeout_millis, DEFAULT_WS_IDLE_TIMEOUT_MILLIS);
}

/// Route-table registration and conflict resolution (ADR-0130 /
/// ADR-0136). These exercise `register_route` / `unregister_route` /
/// `unregister_routes_all` directly against a bare `SharedRoutes` — no
/// chassis, no mail, no boot — so the conflict-resolution branches are
/// pinned deterministically, with no dependence on the order two
/// independent actors' registration mail happens to reach the table.
mod route_registration {
    use super::{
        Arc, KindId, MailboxId, RegisterRouteResult, RwLock, SharedRoutes, register_route, unregister_route,
        unregister_routes_all,
    };
    use crate::kinds::HttpMethod;

    fn fresh_routes() -> SharedRoutes {
        Arc::new(RwLock::new(Vec::new()))
    }

    #[track_caller]
    fn expect_ok(result: RegisterRouteResult) {
        assert!(matches!(result, RegisterRouteResult::Ok), "expected Ok, got {result:?}");
    }

    #[track_caller]
    fn expect_err_containing(result: RegisterRouteResult, needle: &str) {
        match result {
            RegisterRouteResult::Err { error } => {
                assert!(error.contains(needle), "error {error:?} does not contain {needle:?}");
            }
            RegisterRouteResult::Ok => panic!("expected Err containing {needle:?}, got Ok"),
        }
    }

    /// Snapshot the sole route's `(members, kind, shared)`, asserting the
    /// table holds exactly one route — the shape every case below checks.
    fn only_route(routes: &SharedRoutes) -> (Vec<MailboxId>, KindId, bool) {
        let table = routes.read().expect("route table lock");
        assert_eq!(table.len(), 1, "expected exactly one route, got {}", table.len());
        let route = &table[0];
        let snapshot = (route.members.clone(), route.kind, route.shared);
        drop(table);
        snapshot
    }

    /// Tripwire: the exclusive-conflict branch — a second exclusive
    /// claimant of an already-held key is rejected and the first
    /// claimant keeps the route unchanged. This is the boot-order-free
    /// core of the invariant the retired async `conflicting_claim_*`
    /// integration test raced on: the winner is whoever registers first,
    /// full stop, so a deterministic winner comes from ordering the
    /// calls, not from any table-internal tie-break.
    #[test]
    fn exclusive_conflict_first_claimant_keeps_route() {
        let routes = fresh_routes();
        let (first, second) = (MailboxId(1), MailboxId(2));
        let (kind_a, kind_b) = (KindId(100), KindId(200));

        expect_ok(register_route(&routes, "/dup", None, kind_a, first, false));
        expect_err_containing(
            register_route(&routes, "/dup", None, kind_b, second, false),
            "already claimed by mailbox",
        );

        assert_eq!(only_route(&routes), (vec![first], kind_a, false));
    }

    /// Tripwire: the sole-holder idempotent re-claim branch — the same
    /// exclusive mailbox re-registering its own key is `Ok` and updates
    /// `kind` without growing the member set, so a component re-running
    /// `wire` after `replace_component` re-registers cleanly.
    #[test]
    fn exclusive_reclaim_by_holder_updates_kind() {
        let routes = fresh_routes();
        let holder = MailboxId(1);
        let (kind_a, kind_b) = (KindId(100), KindId(200));

        expect_ok(register_route(&routes, "/dup", None, kind_a, holder, false));
        expect_ok(register_route(&routes, "/dup", None, kind_b, holder, false));

        assert_eq!(only_route(&routes), (vec![holder], kind_b, false));
    }

    /// Tripwire: the `(prefix, method)` compound key — a claim on one
    /// method does not conflict with a claim on another method at the
    /// same prefix, so both land as distinct routes.
    #[test]
    fn distinct_method_same_prefix_is_not_a_conflict() {
        let routes = fresh_routes();
        let (a, b) = (MailboxId(1), MailboxId(2));
        let kind = KindId(100);

        expect_ok(register_route(&routes, "/m", Some(HttpMethod::Get), kind, a, false));
        expect_ok(register_route(&routes, "/m", Some(HttpMethod::Post), kind, b, false));

        assert_eq!(routes.read().expect("route table lock").len(), 2);
    }

    /// Tripwire: the shared/exclusive mismatch branch, both directions —
    /// a shared claim cannot join an exclusively-held key, and an
    /// exclusive claim cannot take a shared member set; each rejection
    /// leaves the contested route as its holder(s) left it.
    #[test]
    fn shared_and_exclusive_claims_do_not_mix() {
        // Shared claim onto an exclusive key: rejected, stays exclusive.
        let excl = fresh_routes();
        let (a, b) = (MailboxId(1), MailboxId(2));
        let kind = KindId(100);
        expect_ok(register_route(&excl, "/k", None, kind, a, false));
        expect_err_containing(register_route(&excl, "/k", None, kind, b, true), "exclusively claimed");
        assert_eq!(only_route(&excl), (vec![a], kind, false));

        // Exclusive claim onto a shared key: rejected, stays shared.
        let shared = fresh_routes();
        expect_ok(register_route(&shared, "/k", None, kind, a, true));
        expect_err_containing(register_route(&shared, "/k", None, kind, b, false), "shared member set");
        assert_eq!(only_route(&shared), (vec![a], kind, true));
    }

    /// Tripwire: the kind-mismatch branch on a shared join — a member
    /// registering a different dispatch kind cannot join the set, and
    /// the existing set is untouched.
    #[test]
    fn shared_join_with_mismatched_kind_is_rejected() {
        let routes = fresh_routes();
        let (a, b) = (MailboxId(1), MailboxId(2));
        let (kind_a, kind_b) = (KindId(100), KindId(200));

        expect_ok(register_route(&routes, "/pool", None, kind_a, a, true));
        expect_err_containing(register_route(&routes, "/pool", None, kind_b, b, true), "cannot join");

        assert_eq!(only_route(&routes), (vec![a], kind_a, true));
    }

    /// Tripwire: the shared-join admit branch — a matching shared claim
    /// (same key, same kind) grows the member set in registration order,
    /// and re-registering an existing membership is an idempotent `Ok`
    /// that does not duplicate the member.
    #[test]
    fn matching_shared_claims_accumulate_members() {
        let routes = fresh_routes();
        let (a, b) = (MailboxId(1), MailboxId(2));
        let kind = KindId(100);

        expect_ok(register_route(&routes, "/pool", None, kind, a, true));
        expect_ok(register_route(&routes, "/pool", None, kind, b, true));
        // Idempotent re-registration of an existing member.
        expect_ok(register_route(&routes, "/pool", None, kind, a, true));

        assert_eq!(only_route(&routes), (vec![a, b], kind, true));
    }

    /// Tripwire: unregistration release + drop-when-empty — releasing one
    /// member of a shared set leaves the rest serving, and releasing the
    /// last member drops the route entirely; `unregister_routes_all`
    /// clears every route a mailbox holds.
    #[test]
    fn unregister_releases_members_and_drops_empty_routes() {
        let routes = fresh_routes();
        let (a, b) = (MailboxId(1), MailboxId(2));
        let kind = KindId(100);
        expect_ok(register_route(&routes, "/pool", None, kind, a, true));
        expect_ok(register_route(&routes, "/pool", None, kind, b, true));

        // One member leaves; the set survives with the rest.
        expect_ok(unregister_route(&routes, "/pool", None, a));
        assert_eq!(only_route(&routes), (vec![b], kind, true));

        // The last member leaves; the route is dropped.
        expect_ok(unregister_route(&routes, "/pool", None, b));
        assert!(routes.read().expect("route table lock").is_empty());

        // unregister_routes_all clears every route the mailbox holds.
        expect_ok(register_route(&routes, "/x", None, kind, a, false));
        expect_ok(register_route(&routes, "/y", None, kind, a, false));
        unregister_routes_all(&routes, a);
        assert!(routes.read().expect("route table lock").is_empty());
    }
}

mod shard_startup {
    //! Manual reducer proofs for the startup interleavings. These tests do
    //! not claim scheduler ordering; the loopback server tests exercise the
    //! real owner/activation/task turns.

    use super::super::{
        Arc, HttpServerConfig, HttpSupervisorState, InboundEvent, PendingPeer, ShardSettlement, ShardSlot,
        ShardStartup, WakeSink,
    };
    use crate::kinds::HttpInboundReady;
    use aether_data::{Kind, KindId, MailboxId};
    use aether_substrate::actor::native::NativeCtx;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
    use aether_substrate::mail::{MailId, Source};
    use aether_substrate::testing::boot_authority;
    use std::collections::VecDeque;
    use std::io::Read;
    use std::iter::once;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    fn socket_pair() -> (PendingPeer, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind pending-peer probe");
        let address = listener.local_addr().expect("pending-peer probe address");
        let client = TcpStream::connect(address).expect("connect pending-peer probe");
        let (stream, peer) = listener.accept().expect("accept pending-peer probe");
        (PendingPeer { stream, peer }, client)
    }

    fn sink(registry: &Registry, mailer: &Arc<Mailer>, name: &str) -> (WakeSink, mpsc::Receiver<InboundEvent>) {
        let (inbound_tx, inbound_rx) = mpsc::channel();
        let self_id = registry.register_inbox(
            &boot_authority(),
            name,
            Arc::new(|dispatch: OwnedDispatch| dispatch.discharge()) as Arc<dyn InboxHandler>,
        );
        (
            WakeSink {
                inbound_tx,
                mailer: Arc::clone(mailer),
                self_id,
                wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
                dirty: Arc::new(AtomicBool::new(false)),
            },
            inbound_rx,
        )
    }

    fn starting_state(count: usize, pending_peers: VecDeque<PendingPeer>) -> (Arc<Registry>, HttpSupervisorState) {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let mut state = HttpSupervisorState::disabled(
            HttpServerConfig { enabled: true, max_connections: 8, ..HttpServerConfig::default() },
            mailer,
        );
        state.shard_startup = ShardStartup::Starting {
            remaining: count,
            next_to_stage: None,
            slots_by_index: (0..count).map(|_| ShardSlot::Pending).collect(),
            pending_peers,
        };
        (registry, state)
    }

    fn event_peer(event: InboundEvent) -> SocketAddr {
        let InboundEvent::PeerAccepted { peer, .. } = event else {
            panic!("startup drain posts only PeerAccepted events")
        };
        peer
    }

    /// Out-of-order task completion must not expose the first successful
    /// sink early. The last completion compacts successful indexes in their
    /// configured order, then drains pending sockets FIFO through that stable
    /// round-robin set.
    #[test]
    fn out_of_order_completion_waits_then_drains_fifo_by_index() {
        let (first, first_client) = socket_pair();
        let (second, second_client) = socket_pair();
        let (third, third_client) = socket_pair();
        let expected = [first.peer, second.peer, third.peer];
        let pending_peers = [first, second, third].into_iter().collect();
        let (registry, mut state) = starting_state(3, pending_peers);
        let (sink_zero, rx_zero) = sink(&registry, &state.mailer, "test.http.shard-zero");
        let (sink_two, rx_two) = sink(&registry, &state.mailer, "test.http.shard-two");

        assert!(matches!(state.finish_shard_spawn(2, Some(sink_two)), ShardSettlement::Pending));
        assert!(rx_zero.try_recv().is_err());
        assert!(rx_two.try_recv().is_err(), "a successful shard is not selectable before every attempt settles");

        assert!(matches!(state.finish_shard_spawn(0, Some(sink_zero)), ShardSettlement::Pending));
        let settled = state.finish_shard_spawn(1, None);
        assert!(matches!(settled, ShardSettlement::Ready { shard_count: 2, .. }));
        state.apply_shard_settlement(settled);

        assert_eq!(event_peer(rx_zero.recv().expect("first FIFO peer reaches index zero")), expected[0]);
        assert_eq!(event_peer(rx_two.recv().expect("second FIFO peer reaches index two")), expected[1]);
        assert_eq!(event_peer(rx_zero.recv().expect("third FIFO peer wraps to index zero")), expected[2]);
        assert!(rx_two.try_recv().is_err());
        assert_eq!(state.live_connections.load(Ordering::Acquire), 3);

        drop((first_client, second_client, third_client));
    }

    /// A duplicate or stale task result cannot decrement the attempt count a
    /// second time and therefore cannot transition startup before the real
    /// remaining index settles.
    #[test]
    fn duplicate_completion_cannot_finish_startup_twice() {
        let (registry, mut state) = starting_state(2, VecDeque::new());
        let (sink_zero, _rx_zero) = sink(&registry, &state.mailer, "test.http.duplicate-zero");

        assert!(matches!(state.finish_shard_spawn(0, Some(sink_zero)), ShardSettlement::Pending));
        assert!(matches!(state.finish_shard_spawn(0, None), ShardSettlement::Stale));
        assert!(matches!(state.shard_startup, ShardStartup::Starting { remaining: 1, .. }));
        assert!(matches!(state.finish_shard_spawn(1, None), ShardSettlement::Ready { shard_count: 1, .. }));
    }

    /// When every deterministic child fails, each retained socket receives
    /// the existing controlled `503` and closes. No socket was charged live,
    /// so the global connection count remains balanced.
    #[test]
    fn all_failed_shards_refuse_every_retained_peer() {
        let (pending, mut client) = socket_pair();
        client.set_read_timeout(Some(Duration::from_secs(1))).expect("bound refusal read");
        let (_registry, mut state) = starting_state(1, once(pending).collect());

        let settled = state.finish_shard_spawn(0, None);
        assert!(matches!(settled, ShardSettlement::Failed { .. }));
        state.apply_shard_settlement(settled);

        let mut response = String::new();
        client.read_to_string(&mut response).expect("read controlled startup refusal");
        assert!(response.starts_with("HTTP/1.1 503 "), "expected startup 503, got {response:?}");
        assert_eq!(state.live_connections.load(Ordering::Acquire), 0);

        assert!(matches!(state.shard_startup, ShardStartup::Failed));
    }

    /// Capacity counts supervisor-owned sockets while shard activation is
    /// pending. A second peer is refused immediately and never grows the
    /// pending FIFO or the live-shard count.
    #[test]
    fn pending_peer_counts_toward_the_global_connection_ceiling() {
        let (first, first_client) = socket_pair();
        let (second, mut second_client) = socket_pair();
        second_client.set_read_timeout(Some(Duration::from_secs(1))).expect("bound capacity refusal read");
        let (_registry, mut state) = starting_state(1, once(first).collect());
        state.config.max_connections = 1;
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&state.mailer), MailboxId(0x4067)));
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        state.assign_peer(&mut ctx, second.stream, second.peer);

        let mut response = String::new();
        second_client.read_to_string(&mut response).expect("read pending-capacity refusal");
        assert!(response.starts_with("HTTP/1.1 503 "), "pending peer enforces the ceiling: {response:?}");
        assert!(matches!(
            &state.shard_startup,
            ShardStartup::Starting { pending_peers, .. } if pending_peers.len() == 1
        ));
        assert_eq!(state.live_connections.load(Ordering::Acquire), 0);

        drop(first_client);
    }

    /// Dropping a supervisor during startup drops its one owner of every
    /// retained socket. The peer observes EOF; there is no leaked reader
    /// thread or second socket owner to keep the connection alive.
    #[test]
    fn dropping_starting_state_closes_retained_peer() {
        let (pending, mut client) = socket_pair();
        client.set_read_timeout(Some(Duration::from_secs(1))).expect("bound teardown read");
        let (_registry, state) = starting_state(1, once(pending).collect());

        drop(state);

        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).expect("retained peer observes supervisor teardown");
        assert!(bytes.is_empty(), "teardown closes without fabricating an HTTP response");
    }
}

mod wake_coalescing {
    //! ADR-0135 §4 — the wake-mail coalescing protocol on [`WakeSink`].

    use super::super::{InboundEvent, WakeSink};
    use crate::kinds::HttpInboundReady;
    use aether_data::{Kind, KindId};
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
    use aether_substrate::testing::boot_authority;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

    struct CountingInbox(AtomicUsize);
    impl InboxHandler for CountingInbox {
        fn enqueue(&self, _dispatch: OwnedDispatch) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn sink_with_counter() -> (WakeSink, mpsc::Receiver<InboundEvent>, Arc<CountingInbox>) {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let counter = Arc::new(CountingInbox(AtomicUsize::new(0)));
        let self_id = registry.register_inbox(
            &boot_authority(),
            "test.wake_target",
            Arc::clone(&counter) as Arc<dyn InboxHandler>,
        );
        let (inbound_tx, inbound_rx) = mpsc::channel();
        let sink = WakeSink {
            inbound_tx,
            mailer,
            self_id,
            wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
            dirty: Arc::new(AtomicBool::new(false)),
        };
        (sink, inbound_rx, counter)
    }

    fn probe_event() -> InboundEvent {
        InboundEvent::RequestTimedOut { conn_id: 0 }
    }

    /// Tripwire: a burst of posts between drains fires exactly one wake
    /// mail — without the dirty-flag swap, every post fires one (the
    /// pre-ADR-0135 per-event wake volume this optimization exists to
    /// remove).
    #[test]
    fn burst_fires_one_wake() {
        let (sink, _rx, counter) = sink_with_counter();
        for _ in 0..16 {
            assert!(sink.post(probe_event()));
        }
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }

    /// Tripwire: the drain-side arm order (clear the flag *before*
    /// draining) means a post landing mid-drain re-fires the wake —
    /// clearing after the drain instead would swallow it and strand the
    /// event until the next unrelated wake. `_dead_mailbox_id` never
    /// aliases; the second wake is observable as a second count.
    #[test]
    fn post_after_arm_refires_wake() {
        let (sink, rx, counter) = sink_with_counter();
        assert!(sink.post(probe_event()));
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);

        // Drain begins: arm first (the load-bearing order), then empty
        // the channel.
        WakeSink::arm_for_drain(&sink.dirty);
        while rx.try_recv().is_ok() {}

        // A post after the arm — even mid-drain — must fire a fresh
        // wake, or the event would sit undelivered.
        assert!(sink.post(probe_event()));
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
    }
}
