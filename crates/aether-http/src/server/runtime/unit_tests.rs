use super::{
    Arc, HttpResponseStreamOpen, HttpServerCapability, HttpServerConfig, HttpVersion, OPCODE_BINARY,
    OPCODE_CONTINUATION, OPCODE_TEXT, RegisterRouteResult, RwLock, SharedRoutes, WsFrameParse, http_date,
    normalize_prefix, parse_http_method, parse_ws_frame, percent_decode_path, reason_phrase, register_route,
    render_stream_head, request_keeps_alive, sec_websocket_accept, serialize_ws_frame, sha1, unregister_route,
    unregister_routes_all, validate_ws_handshake,
};
use crate::kinds::{HttpHeader, HttpMethod, MethodFilter, RegisterRouteError};
use crate::typed::{route_matches, route_rank};
use aether_actor::ErasedActorRef;
use aether_substrate::actor::native::PumpedSlot;
use aether_substrate::chassis::builder::PassiveChassis;
use aether_substrate::mail::outbound::EgressEvent;
use aether_substrate::testing::{TestChassis, boot_bare_test_chassis, fresh_substrate_and_rx};
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

/// The supervisor booted as a pumped actor on a bare `TestChassis`: nothing
/// on its inbox runs until the test drains it, and a reducer the supervisor
/// runs in its own handler turns runs here in a real host turn, under the
/// actor's own binding and the chassis spawner.
struct Supervisor {
    /// Declared before `slot` so it drops first: a chassis goes before the
    /// pumped root it hosts, which closes when its slot drops.
    chassis: PassiveChassis<TestChassis>,
    slot: PumpedSlot<HttpServerCapability>,
    egress: mpsc::Receiver<EgressEvent>,
}

/// Boot the supervisor under `config`. Every caller composes it disabled, so
/// it binds no socket and spawns no accept thread; a test that needs startup
/// state seeds it inside a host turn.
fn boot_supervisor(config: HttpServerConfig) -> Supervisor {
    let (registry, mailer, egress) = fresh_substrate_and_rx();
    let chassis = boot_bare_test_chassis(&registry, &mailer);
    let (slot, _wake) =
        chassis.boot_pumped_actor::<HttpServerCapability>(config, ()).expect("the http supervisor boots pumped");

    Supervisor { chassis, slot, egress }
}

fn conn_header(value: &str) -> Vec<HttpHeader> {
    vec![HttpHeader { name: "Connection".to_string(), value: value.to_string() }]
}

/// ADR-0155 §3: a server composed disabled claims its mailbox but binds
/// no socket, so its route-registration surface must fail fast with an
/// `Err` reply rather than the mail warn-dropping at an unknown mailbox. The
/// request names a live `HttpRouter` holder, so the mail decodes and reaches the handler:
/// the bug this catches is the disabled branch no longer answering.
#[test]
fn disabled_http_server_err_replies_to_register_route() {
    use crate::kinds::{HttpRouter, RegisterRoute};
    use crate::server::tests::handlers::EchoHttpHandler;
    use aether_actor::ActorPath;
    use aether_data::{SessionToken, Uuid};
    use aether_substrate::ReplyTarget;
    use aether_substrate::testing::decode_session_reply;

    let mut supervisor = boot_supervisor(HttpServerConfig::default());
    let (_holder, _wake) =
        supervisor.chassis.boot_pumped_actor::<EchoHttpHandler>((), ()).expect("the route holder boots pumped");
    let register = RegisterRoute {
        prefix: "/".to_string(),
        method: MethodFilter::Any,
        handler: ActorPath::<EchoHttpHandler>::root().narrow::<HttpRouter>(),
        shared: false,
    };
    let reply = ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0x7043)), correlation: 1 };

    supervisor.chassis.send_for_reply(supervisor.chassis.actor_ref::<HttpServerCapability>(), &register, reply);
    supervisor.slot.drain_available();

    let result: RegisterRouteResult = decode_session_reply(&supervisor.egress);
    assert!(
        matches!(&result, RegisterRouteResult::Err(RegisterRouteError::Rejected { error }) if error.contains("disabled")),
        "a disabled http server must fail fast on register_route, got {result:?}",
    );
}

/// Tripwire: keep-alive defaulting is branch logic over the HTTP version
/// and the `Connection` header, not a derived mirror — HTTP/1.1 keeps
/// alive unless told to close, HTTP/1.0 closes unless told to keep alive,
/// and an explicit token wins over the version default either way.
#[test]
fn keep_alive_defaults_by_version_and_connection_header() {
    // HTTP/1.1: keep-alive by default, `close` overrides.
    assert!(request_keeps_alive(HttpVersion::Http11, &[]));
    assert!(!request_keeps_alive(HttpVersion::Http11, &conn_header("close")));
    assert!(request_keeps_alive(HttpVersion::Http11, &conn_header("keep-alive")));
    // HTTP/1.0: close by default, `keep-alive` overrides.
    assert!(!request_keeps_alive(HttpVersion::Http10, &[]));
    assert!(request_keeps_alive(HttpVersion::Http10, &conn_header("keep-alive")));
    assert!(!request_keeps_alive(HttpVersion::Http10, &conn_header("close")));
    // Case-insensitive, and a token among comma-separated values counts.
    assert!(!request_keeps_alive(HttpVersion::Http11, &conn_header("Close")));
    assert!(request_keeps_alive(HttpVersion::Http10, &conn_header("keep-alive, Upgrade")));
    assert!(!request_keeps_alive(HttpVersion::Unknown, &[]));
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

/// Conversion-risk: `MethodFilter::Any` matches every method while `Only`
/// matches its own — swapped arms would invert the rank, so this pins
/// parity across every method plus prefix match/mismatch with computed
/// expectations.
#[test]
fn method_filter_rank_parity() {
    for method in [
        HttpMethod::Get,
        HttpMethod::Post,
        HttpMethod::Put,
        HttpMethod::Delete,
        HttpMethod::Patch,
        HttpMethod::Head,
        HttpMethod::Options,
    ] {
        let prefix_len = "/api".len();
        assert_eq!(route_rank("/api", MethodFilter::Any, "/api", method), Some((prefix_len, false)));
        assert_eq!(route_rank("/api", MethodFilter::Only(method), "/api", method), Some((prefix_len, true)));
        let other = if method == HttpMethod::Get {
            HttpMethod::Post
        } else {
            HttpMethod::Get
        };
        assert_eq!(route_rank("/api", MethodFilter::Only(method), "/api", other), None);
    }
    assert_eq!(route_rank("/api", MethodFilter::Any, "/other", HttpMethod::Get), None);
    assert_eq!(route_rank("/api", MethodFilter::Only(HttpMethod::Get), "/apiary", HttpMethod::Get), None);
    assert_eq!(route_rank("/", MethodFilter::Any, "/anything", HttpMethod::Get), Some((1, false)));
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
    use super::super::{RequestStreamSupport, RouteMember, RouteTable, StreamCreditSupport, WebSocketSupport};
    use super::{
        Arc, ErasedActorRef, RegisterRouteResult, RwLock, SharedRoutes, register_route, unregister_route,
        unregister_routes_all,
    };
    use crate::kinds::{HttpMethod, HttpRouter, MethodFilter, RegisterRouteError};
    use crate::server::tests::handlers::router_holders as holders;
    use aether_actor::ProtocolRef;

    fn fresh_routes() -> SharedRoutes {
        Arc::new(RwLock::new(RouteTable::default()))
    }

    /// The route member for a holder that covers only `HttpRouter`, as the
    /// fixture handlers do: no data-phase support.
    fn member(router: ProtocolRef<HttpRouter>) -> RouteMember {
        RouteMember {
            router,
            credit: StreamCreditSupport::Unsupported,
            request_stream: RequestStreamSupport::Unsupported,
            websocket: WebSocketSupport::Unsupported,
        }
    }

    #[track_caller]
    fn expect_ok(result: RegisterRouteResult) {
        assert!(matches!(result, RegisterRouteResult::Ok), "expected Ok, got {result:?}");
    }

    #[track_caller]
    fn expect_err_containing(result: RegisterRouteResult, needle: &str) {
        match result {
            RegisterRouteResult::Err(RegisterRouteError::Rejected { error }) => {
                assert!(error.contains(needle), "error {error:?} does not contain {needle:?}");
            }
            other => panic!("expected a rejection containing {needle:?}, got {other:?}"),
        }
    }

    /// Snapshot the sole route's `(members, shared)`, members by identity,
    /// asserting the table holds exactly one route — the shape every case
    /// below checks.
    fn only_route(routes: &SharedRoutes) -> (Vec<ErasedActorRef>, bool) {
        let table = routes.read().expect("route table lock");
        assert_eq!(table.routes.len(), 1, "expected exactly one route, got {}", table.routes.len());
        let route = table.routes.values().next().expect("one route");
        let snapshot = (route.members.iter().map(|member| member.router.erase()).collect(), route.shared);
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
        let (_chassis, first, second) = holders();

        expect_ok(register_route(&routes, "/dup", MethodFilter::Any, member(first), false));
        expect_err_containing(
            register_route(&routes, "/dup", MethodFilter::Any, member(second), false),
            "already claimed by",
        );

        assert_eq!(only_route(&routes), (vec![first.erase()], false));
    }

    /// Tripwire: the sole-holder idempotent re-claim branch — the same
    /// exclusive holder re-registering its own key is `Ok` without growing
    /// the member set, so a component re-running `wire` after
    /// `replace_component` re-registers cleanly.
    #[test]
    fn exclusive_reclaim_by_holder_is_idempotent() {
        let routes = fresh_routes();
        let (_chassis, holder, _) = holders();

        expect_ok(register_route(&routes, "/dup", MethodFilter::Any, member(holder), false));
        expect_ok(register_route(&routes, "/dup", MethodFilter::Any, member(holder), false));

        assert_eq!(only_route(&routes), (vec![holder.erase()], false));
    }

    /// Tripwire: the `(prefix, method)` compound key — a claim on one
    /// method does not conflict with a claim on another method at the
    /// same prefix, so both land as distinct routes.
    #[test]
    fn distinct_method_same_prefix_is_not_a_conflict() {
        let routes = fresh_routes();
        let (_chassis, a, b) = holders();

        expect_ok(register_route(&routes, "/m", MethodFilter::Only(HttpMethod::Get), member(a), false));
        expect_ok(register_route(&routes, "/m", MethodFilter::Only(HttpMethod::Post), member(b), false));

        assert_eq!(routes.read().expect("route table lock").routes.len(), 2);
    }

    /// Tripwire: the shared/exclusive mismatch branch, both directions —
    /// a shared claim cannot join an exclusively-held key, and an
    /// exclusive claim cannot take a shared member set; each rejection
    /// leaves the contested route as its holder(s) left it.
    #[test]
    fn shared_and_exclusive_claims_do_not_mix() {
        // Shared claim onto an exclusive key: rejected, stays exclusive.
        let excl = fresh_routes();
        let (_chassis, a, b) = holders();
        expect_ok(register_route(&excl, "/k", MethodFilter::Any, member(a), false));
        expect_err_containing(register_route(&excl, "/k", MethodFilter::Any, member(b), true), "exclusively claimed");
        assert_eq!(only_route(&excl), (vec![a.erase()], false));

        // Exclusive claim onto a shared key: rejected, stays shared.
        let shared = fresh_routes();
        expect_ok(register_route(&shared, "/k", MethodFilter::Any, member(a), true));
        expect_err_containing(register_route(&shared, "/k", MethodFilter::Any, member(b), false), "shared member set");
        assert_eq!(only_route(&shared), (vec![a.erase()], true));
    }

    /// Tripwire: the shared-join admit branch — a matching shared claim
    /// grows the member set in registration order, and re-registering an
    /// existing membership is an idempotent `Ok` that does not duplicate
    /// the member.
    #[test]
    fn matching_shared_claims_accumulate_members() {
        let routes = fresh_routes();
        let (_chassis, a, b) = holders();

        expect_ok(register_route(&routes, "/pool", MethodFilter::Any, member(a), true));
        expect_ok(register_route(&routes, "/pool", MethodFilter::Any, member(b), true));
        // Idempotent re-registration of an existing member.
        expect_ok(register_route(&routes, "/pool", MethodFilter::Any, member(a), true));

        assert_eq!(only_route(&routes), (vec![a.erase(), b.erase()], true));

        // A shared join is recorded in the reverse index: releasing every
        // route `a` holds leaves `b` as the set's sole member.
        unregister_routes_all(&routes, a.erase());
        assert_eq!(only_route(&routes), (vec![b.erase()], true));
    }

    /// Tripwire: unregistration release + drop-when-empty — releasing one
    /// member of a shared set leaves the rest serving, and releasing the
    /// last member drops the route entirely; `unregister_routes_all`
    /// clears every route a mailbox holds.
    #[test]
    fn unregister_releases_members_and_drops_empty_routes() {
        let routes = fresh_routes();
        let (_chassis, a, b) = holders();
        expect_ok(register_route(&routes, "/pool", MethodFilter::Any, member(a), true));
        expect_ok(register_route(&routes, "/pool", MethodFilter::Any, member(b), true));

        // One member leaves; the set survives with the rest.
        expect_ok(unregister_route(&routes, "/pool", MethodFilter::Any, a.erase()));
        assert_eq!(only_route(&routes), (vec![b.erase()], true));

        // The last member leaves; the route is dropped.
        expect_ok(unregister_route(&routes, "/pool", MethodFilter::Any, b.erase()));
        assert!(routes.read().expect("route table lock").routes.is_empty());

        // unregister_routes_all clears every route the holder holds.
        expect_ok(register_route(&routes, "/x", MethodFilter::Any, member(a), false));
        expect_ok(register_route(&routes, "/y", MethodFilter::Any, member(a), false));
        unregister_routes_all(&routes, a.erase());
        assert!(routes.read().expect("route table lock").routes.is_empty());
    }
}

mod shard_startup {
    //! Reducer proofs for the startup interleavings, run in the booted
    //! supervisor's own host turns. These tests do not claim scheduler
    //! ordering; the loopback server tests exercise the real
    //! owner/activation/task turns.

    use super::super::{
        Arc, HttpDispatchShard, HttpServerConfig, HttpSupervisorState, InboundEvent, PendingPeer, ShardSettlement,
        ShardSink, ShardSlot, ShardSpawnOutcome, ShardStartup, StageCursor,
    };
    use super::{Supervisor, boot_supervisor};
    use aether_substrate::Subname;
    use aether_substrate::chassis::builder::PassiveChassis;
    use aether_substrate::testing::TestChassis;
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

    /// A shard sink over a test-owned channel, joined with the proof of a
    /// real dispatch shard spawned from the seed the supervisor builds, as the
    /// shard's `SpawnOutcome` hands the supervisor its proof. The shard drains
    /// its own channel, so every event a test posts stays on the returned
    /// receiver.
    fn sink(chassis: &PassiveChassis<TestChassis>, subname: &str) -> (ShardSink, mpsc::Receiver<InboundEvent>) {
        let (inbound_tx, inbound_rx) = mpsc::channel();
        let (seed, _channel) = HttpSupervisorState::disabled(config(8)).shard_seed();
        let shard = chassis
            .spawn_actor_for_test::<HttpDispatchShard>(Subname::Named(subname), seed, ())
            .finish()
            .expect("the test dispatch shard spawns");

        (ShardSink { inbound_tx, dirty: Arc::new(AtomicBool::new(false)), shard }, inbound_rx)
    }

    fn starting(count: usize, pending_peers: VecDeque<PendingPeer>) -> ShardStartup {
        ShardStartup::Starting {
            remaining: count,
            next_to_stage: StageCursor::Exhausted,
            slots_by_index: (0..count).map(|_| ShardSlot::Pending).collect(),
            pending_peers,
        }
    }

    fn config(max_connections: usize) -> HttpServerConfig {
        HttpServerConfig { max_connections, ..HttpServerConfig::default() }
    }

    /// A bare supervisor state mid-startup, for the reducers that take no ctx.
    fn starting_state(count: usize, pending_peers: VecDeque<PendingPeer>) -> HttpSupervisorState {
        let mut state = HttpSupervisorState::disabled(config(8));
        state.shard_startup = starting(count, pending_peers);
        state
    }

    /// Boot the supervisor and seed its startup state in a host turn, the
    /// shape `assign_peer` leaves behind after the first accepted peer.
    fn booted_starting(max_connections: usize, count: usize, pending_peers: VecDeque<PendingPeer>) -> Supervisor {
        let mut supervisor = boot_supervisor(config(max_connections));
        supervisor.slot.host_turn(|state, _ctx| state.shard_startup = starting(count, pending_peers));
        supervisor
    }

    fn live_connections(supervisor: &Supervisor) -> usize {
        supervisor.slot.read_state(|state| state.live_connections.load(Ordering::Acquire)).expect("supervisor is live")
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
        let mut supervisor = booted_starting(8, 3, [first, second, third].into_iter().collect());
        let (sink_zero, rx_zero) = sink(&supervisor.chassis, "test-zero");
        let (sink_two, rx_two) = sink(&supervisor.chassis, "test-two");

        let early = supervisor.slot.host_turn(|state, ctx| {
            let settled = state.finish_shard_spawn(2, ShardSpawnOutcome::Ready(sink_two));
            let pending = matches!(settled, ShardSettlement::Pending);
            state.apply_shard_settlement(ctx, settled);
            pending
        });
        assert_eq!(early, Some(true));
        assert!(rx_zero.try_recv().is_err());
        assert!(rx_two.try_recv().is_err(), "a successful shard is not selectable before every attempt settles");

        let last = supervisor.slot.host_turn(|state, ctx| {
            let middle = state.finish_shard_spawn(0, ShardSpawnOutcome::Ready(sink_zero));
            let middle_pending = matches!(middle, ShardSettlement::Pending);
            state.apply_shard_settlement(ctx, middle);
            let settled = state.finish_shard_spawn(1, ShardSpawnOutcome::Failed);
            let ready = matches!(settled, ShardSettlement::Ready { shard_count: 2, .. });
            state.apply_shard_settlement(ctx, settled);
            (middle_pending, ready)
        });
        assert_eq!(last, Some((true, true)));

        assert_eq!(event_peer(rx_zero.recv().expect("first FIFO peer reaches index zero")), expected[0]);
        assert_eq!(event_peer(rx_two.recv().expect("second FIFO peer reaches index two")), expected[1]);
        assert_eq!(event_peer(rx_zero.recv().expect("third FIFO peer wraps to index zero")), expected[2]);
        assert!(rx_two.try_recv().is_err());
        assert_eq!(live_connections(&supervisor), 3);

        drop((first_client, second_client, third_client));
    }

    /// A duplicate or stale task result cannot decrement the attempt count a
    /// second time and therefore cannot transition startup before the real
    /// remaining index settles.
    #[test]
    fn duplicate_completion_cannot_finish_startup_twice() {
        let supervisor = boot_supervisor(config(8));
        let mut state = starting_state(2, VecDeque::new());
        let (sink_zero, _rx_zero) = sink(&supervisor.chassis, "test-duplicate-zero");

        assert!(matches!(state.finish_shard_spawn(0, ShardSpawnOutcome::Ready(sink_zero)), ShardSettlement::Pending));
        assert!(matches!(state.finish_shard_spawn(0, ShardSpawnOutcome::Failed), ShardSettlement::Stale));
        assert!(matches!(state.shard_startup, ShardStartup::Starting { remaining: 1, .. }));
        assert!(matches!(
            state.finish_shard_spawn(1, ShardSpawnOutcome::Failed),
            ShardSettlement::Ready { shard_count: 1, .. }
        ));
    }

    /// When every deterministic child fails, each retained socket receives
    /// the existing controlled `503` and closes. No socket was charged live,
    /// so the global connection count remains balanced.
    #[test]
    fn all_failed_shards_refuse_every_retained_peer() {
        let (pending, mut client) = socket_pair();
        client.set_read_timeout(Some(Duration::from_secs(1))).expect("bound refusal read");
        let mut supervisor = booted_starting(8, 1, once(pending).collect());

        let failed = supervisor.slot.host_turn(|state, ctx| {
            let settled = state.finish_shard_spawn(0, ShardSpawnOutcome::Failed);
            let failed = matches!(settled, ShardSettlement::Failed { .. });
            state.apply_shard_settlement(ctx, settled);
            failed
        });
        assert_eq!(failed, Some(true));

        let mut response = String::new();
        client.read_to_string(&mut response).expect("read controlled startup refusal");
        assert!(response.starts_with("HTTP/1.1 503 "), "expected startup 503, got {response:?}");
        assert_eq!(live_connections(&supervisor), 0);

        let startup_failed = supervisor.slot.read_state(|state| matches!(state.shard_startup, ShardStartup::Failed));
        assert_eq!(startup_failed, Some(true));
    }

    /// Capacity counts supervisor-owned sockets while shard activation is
    /// pending. A second peer is refused immediately and never grows the
    /// pending FIFO or the live-shard count.
    #[test]
    fn pending_peer_counts_toward_the_global_connection_ceiling() {
        let (first, first_client) = socket_pair();
        let (second, mut second_client) = socket_pair();
        second_client.set_read_timeout(Some(Duration::from_secs(1))).expect("bound capacity refusal read");
        let mut supervisor = booted_starting(1, 1, once(first).collect());

        supervisor.slot.host_turn(|state, ctx| state.assign_peer(ctx, second.stream, second.peer));

        let mut response = String::new();
        second_client.read_to_string(&mut response).expect("read pending-capacity refusal");
        assert!(response.starts_with("HTTP/1.1 503 "), "pending peer enforces the ceiling: {response:?}");
        let pending = supervisor.slot.read_state(|state| match &state.shard_startup {
            ShardStartup::Starting { pending_peers, .. } => Some(pending_peers.len()),
            ShardStartup::Idle | ShardStartup::Ready { .. } | ShardStartup::Failed => None,
        });
        assert_eq!(pending, Some(Some(1)));
        assert_eq!(live_connections(&supervisor), 0);

        drop(first_client);
    }

    /// Conversion-risk: the staging cursor must stage each index once, in
    /// order, then exhaust — a cursor that re-stages or skips would either
    /// double-stage an index or leave a slot pending, so this drives
    /// `stage_next_shard` through each index to `Exhausted` then false.
    #[test]
    fn staging_cursor_stages_each_index_once_then_exhausts() {
        let mut supervisor = boot_supervisor(config(8));
        supervisor.slot.host_turn(|state, _ctx| {
            state.shard_startup = starting(2, VecDeque::new());
            if let ShardStartup::Starting { next_to_stage, .. } = &mut state.shard_startup {
                *next_to_stage = StageCursor::Next(0);
            }
        });

        let first = supervisor.slot.host_turn(|state, ctx| {
            let staged = state.stage_next_shard(ctx);
            let cursor = match &state.shard_startup {
                ShardStartup::Starting { next_to_stage: StageCursor::Next(index), .. } => Some(*index),
                ShardStartup::Starting { next_to_stage: StageCursor::Exhausted, .. }
                | ShardStartup::Idle
                | ShardStartup::Ready { .. }
                | ShardStartup::Failed => None,
            };
            let staged_zero = match &state.shard_startup {
                ShardStartup::Starting { slots_by_index, .. } => {
                    let first_slot = slots_by_index.first();
                    matches!(first_slot, Some(ShardSlot::Staged(_)))
                }
                ShardStartup::Idle | ShardStartup::Ready { .. } | ShardStartup::Failed => false,
            };
            (staged, cursor, staged_zero)
        });
        assert_eq!(first, Some((true, Some(1), true)));

        let second = supervisor.slot.host_turn(|state, ctx| {
            let staged = state.stage_next_shard(ctx);
            let exhausted =
                matches!(state.shard_startup, ShardStartup::Starting { next_to_stage: StageCursor::Exhausted, .. });
            let staged_one = match &state.shard_startup {
                ShardStartup::Starting { slots_by_index, .. } => {
                    let second_slot = slots_by_index.get(1);
                    matches!(second_slot, Some(ShardSlot::Staged(_)))
                }
                ShardStartup::Idle | ShardStartup::Ready { .. } | ShardStartup::Failed => false,
            };
            (staged, exhausted, staged_one)
        });
        assert_eq!(second, Some((true, true, true)));

        let third = supervisor.slot.host_turn(HttpSupervisorState::stage_next_shard);
        assert_eq!(third, Some(false));
    }

    /// Dropping a supervisor during startup drops its one owner of every
    /// retained socket. The peer observes EOF; there is no leaked reader
    /// thread or second socket owner to keep the connection alive.
    #[test]
    fn dropping_starting_state_closes_retained_peer() {
        let (pending, mut client) = socket_pair();
        client.set_read_timeout(Some(Duration::from_secs(1))).expect("bound teardown read");
        let state = starting_state(1, once(pending).collect());

        drop(state);

        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).expect("retained peer observes supervisor teardown");
        assert!(bytes.is_empty(), "teardown closes without fabricating an HTTP response");
    }
}

mod wake_coalescing {
    //! ADR-0135 §4 — the wake-mail coalescing protocol on [`WakeSink`],
    //! counted as the wakes a booted actor receives.

    use super::super::{InboundEvent, WakeSink};
    use crate::kinds::HttpInboundReady;
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, PumpedSlot};
    use aether_substrate::chassis::builder::PassiveChassis;
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::testing::{TestChassis, boot_bare_test_chassis, fresh_substrate};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    /// Counts the `HttpInboundReady` wakes dispatched to it: the stand-in for
    /// the supervisor or shard a sink wakes.
    struct WakeCounter {
        wakes: usize,
    }

    #[actor(singleton, root)]
    impl NativeActor for WakeCounter {
        const NAMESPACE: &'static str = "test.http.wake_counter";
        type Config = ();

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wakes: 0 })
        }

        #[handler::tell]
        fn on_wake(&mut self, _ctx: &mut NativeCtx<'_>, _wake: HttpInboundReady) {
            self.wakes += 1;
        }
    }

    /// The counter booted pumped, and a sink whose wake it minted in a host
    /// turn, as `init` mints the supervisor's.
    struct Counted {
        /// Declared before `slot` so it drops first: a chassis goes before
        /// the pumped root it hosts.
        _chassis: PassiveChassis<TestChassis>,
        slot: PumpedSlot<WakeCounter>,
        sink: WakeSink,
        inbound_rx: mpsc::Receiver<InboundEvent>,
    }

    impl Counted {
        /// Dispatch every queued wake, then read how many the counter has seen.
        fn drained_wakes(&mut self) -> usize {
            self.slot.drain_available();
            self.slot.read_state(|counter| counter.wakes).expect("counter is live")
        }
    }

    fn counted_sink() -> Counted {
        let (registry, mailer) = fresh_substrate();
        let chassis = boot_bare_test_chassis(&registry, &mailer);
        let (mut slot, _wake) =
            chassis.boot_pumped_actor::<WakeCounter>((), ()).expect("the wake counter boots pumped");
        let wake = slot.host_turn(|_, ctx| ctx.self_wake::<HttpInboundReady>()).expect("counter is live");
        let (inbound_tx, inbound_rx) = mpsc::channel();
        let sink = WakeSink { inbound_tx, wake, dirty: Arc::new(AtomicBool::new(false)) };

        Counted { _chassis: chassis, slot, sink, inbound_rx }
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
        let mut counted = counted_sink();
        for _ in 0..16 {
            assert!(counted.sink.post(probe_event()));
        }

        assert_eq!(counted.drained_wakes(), 1);
    }

    /// Tripwire: the drain-side arm order (clear the flag *before*
    /// draining) means a post landing mid-drain re-fires the wake —
    /// clearing after the drain instead would swallow it and strand the
    /// event until the next unrelated wake.
    #[test]
    fn post_after_arm_refires_wake() {
        let mut counted = counted_sink();
        assert!(counted.sink.post(probe_event()));
        assert_eq!(counted.drained_wakes(), 1);

        // Drain begins: arm first (the load-bearing order), then empty
        // the channel.
        WakeSink::arm_for_drain(&counted.sink.dirty);
        while counted.inbound_rx.try_recv().is_ok() {}

        // A post after the arm — even mid-drain — must fire a fresh
        // wake, or the event would sit undelivered.
        assert!(counted.sink.post(probe_event()));
        assert_eq!(counted.drained_wakes(), 2);
    }
}
