use super::{HttpServerCapability, HttpServerConfig, HttpServerHandle};
use crate::trace::TraceDispatchCapability;
use aether_actor::Addressable;
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::testing::{TestChassis, fresh_substrate};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use test_handlers::{
    ApiRouteHandler, ApiV2Handler, DupFirstHandler, DupSecondHandler, EchoHttpHandler,
    FixedBodyHttpHandler, FloodHttpHandler, MethodAnyHandler, MethodPostHandler,
    STREAM_CHUNK_COUNT, SilentHttpHandler, StreamHttpHandler, TmpRouteHandler, stream_chunk_body,
};

mod test_handlers {
    //! Minimal native handler actors behind the server in the integration
    //! tests: one that replies `200` echoing the request, one that drops
    //! the request without replying (the `502` safety-net path), two
    //! response-streaming handlers (ADR-0128) — a well-behaved one that
    //! paces chunks against credit, and a flooder that ignores credit —
    //! plus the ADR-0130 routed handlers that claim prefixes from `wire`
    //! via `register_route_self` — the same registration path a component
    //! takes — and reply fixed tags so a test can assert which handler a
    //! request reached.
    use aether_actor::actor;
    use aether_data::Kind;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    use crate::http::kinds::{
        HttpHeader, HttpMethod, HttpResponseChunk, HttpResponseStreamEnd, HttpResponseStreamOpen,
        HttpServerRequest, HttpServerResponse, HttpStreamCredit, RegisterRouteSelf,
        UnregisterRouteSelf,
    };
    use crate::http::server::HttpServerCapability;

    /// A minted route kind (ADR-0130): the same shape as
    /// [`HttpServerRequest`] under a distinct kind name, so the cap's
    /// route-as-kind dispatch stamps this id and the handler's typed
    /// `#[handler]` decodes the request-shaped payload as it.
    #[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize)]
    #[kind(name = "aether.http.test.api_route_request")]
    pub struct ApiRouteRequest {
        pub method: HttpMethod,
        pub path: String,
        pub query: String,
        pub headers: Vec<HttpHeader>,
        pub body: Vec<u8>,
        pub peer_addr: String,
    }

    /// Claims `/api` from `wire` with the minted [`ApiRouteRequest`]
    /// kind (registering the same key twice to pin the idempotent
    /// same-mailbox re-claim), and echoes the decoded path back in the
    /// body — proving the routed payload decoded as the registered
    /// kind, not just that dispatch picked the right mailbox.
    pub struct ApiRouteHandler;
    pub struct ApiRouteHandlerState;

    #[actor(singleton)]
    impl NativeActor for ApiRouteHandler {
        type State = ApiRouteHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_route_api";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<ApiRouteHandlerState, BootError> {
            Ok(ApiRouteHandlerState)
        }

        fn wire(_state: &mut ApiRouteHandlerState, ctx: &mut NativeCtx<'_>) {
            let claim = RegisterRouteSelf {
                prefix: "/api".to_string(),
                method: None,
                kind: <ApiRouteRequest as Kind>::ID,
            };
            ctx.actor::<HttpServerCapability>().send(&claim);
            // Same mailbox re-claiming its own key: idempotent Ok.
            ctx.actor::<HttpServerCapability>().send(&claim);
        }

        #[handler]
        fn on_request(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            request: ApiRouteRequest,
        ) -> HttpServerResponse {
            HttpServerResponse {
                status: 200,
                headers: Vec::new(),
                body: format!("api:{}", request.path).into_bytes(),
            }
        }
    }

    /// Claims `/tmp` from `wire`; on any request it releases its own
    /// route via `unregister_route_self` before replying, so the next
    /// request to `/tmp` falls back to the default handler.
    pub struct TmpRouteHandler;
    pub struct TmpRouteHandlerState;

    #[actor(singleton)]
    impl NativeActor for TmpRouteHandler {
        type State = TmpRouteHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_route_tmp";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<TmpRouteHandlerState, BootError> {
            Ok(TmpRouteHandlerState)
        }

        fn wire(_state: &mut TmpRouteHandlerState, ctx: &mut NativeCtx<'_>) {
            ctx.actor::<HttpServerCapability>()
                .send(&RegisterRouteSelf {
                    prefix: "/tmp".to_string(),
                    method: None,
                    kind: <HttpServerRequest as Kind>::ID,
                });
        }

        #[handler]
        fn on_request(
            _state: &mut Self::State,
            ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpServerResponse {
            ctx.actor::<HttpServerCapability>()
                .send(&UnregisterRouteSelf {
                    prefix: "/tmp".to_string(),
                    method: None,
                });
            HttpServerResponse {
                status: 200,
                headers: Vec::new(),
                body: b"tmp".to_vec(),
            }
        }
    }

    /// A routed handler that claims its prefixes from `wire` with the
    /// generic request kind and replies `200` with a fixed tag body.
    macro_rules! routed_handler {
        ($ty:ident, $state:ident, $namespace:literal, $tag:literal,
         [$(($method:expr, $prefix:literal)),+ $(,)?]) => {
            pub struct $ty;
            pub struct $state;

            #[actor(singleton)]
            impl NativeActor for $ty {
                type State = $state;
                type Config = ();
                const NAMESPACE: &'static str = $namespace;

                fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<$state, BootError> {
                    Ok($state)
                }

                fn wire(_state: &mut $state, ctx: &mut NativeCtx<'_>) {
                    $(ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
                        prefix: $prefix.to_string(),
                        method: $method,
                        kind: <HttpServerRequest as Kind>::ID,
                    });)+
                }

                #[handler]
                fn on_request(
                    _state: &mut Self::State,
                    _ctx: &mut NativeCtx<'_>,
                    _request: HttpServerRequest,
                ) -> HttpServerResponse {
                    HttpServerResponse {
                        status: 200,
                        headers: Vec::new(),
                        body: $tag.to_vec(),
                    }
                }
            }
        };
    }

    routed_handler!(
        ApiV2Handler,
        ApiV2HandlerState,
        "aether.http.test_route_api_v2",
        b"api-v2",
        [(None, "/api/v2")]
    );
    routed_handler!(
        MethodPostHandler,
        MethodPostHandlerState,
        "aether.http.test_route_post_m",
        b"post-m",
        [(Some(HttpMethod::Post), "/m")]
    );
    routed_handler!(
        MethodAnyHandler,
        MethodAnyHandlerState,
        "aether.http.test_route_any_m",
        b"any-m",
        [(None, "/m")]
    );
    routed_handler!(
        DupFirstHandler,
        DupFirstHandlerState,
        "aether.http.test_route_dup_first",
        b"first",
        [(None, "/dup")]
    );
    routed_handler!(
        DupSecondHandler,
        DupSecondHandlerState,
        "aether.http.test_route_dup_second",
        b"second",
        [(None, "/dup"), (None, "/second")]
    );

    /// Replies `200` and echoes the request's method / path / query /
    /// peer address (as headers) and body (verbatim), so a test can
    /// assert the full request round-tripped to the handler.
    pub struct EchoHttpHandler;

    /// Empty runtime state for the stateless echo handler (ADR-0122: a
    /// stateless cap still names a state type rather than `()` / `Self`).
    pub struct EchoHttpHandlerState;

    #[actor(singleton)]
    impl NativeActor for EchoHttpHandler {
        type State = EchoHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_echo_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<EchoHttpHandlerState, BootError> {
            Ok(EchoHttpHandlerState)
        }

        #[handler]
        fn on_request(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            request: HttpServerRequest,
        ) -> HttpServerResponse {
            let headers = vec![
                HttpHeader {
                    name: "x-aether-method".to_string(),
                    value: format!("{:?}", request.method),
                },
                HttpHeader {
                    name: "x-aether-path".to_string(),
                    value: request.path.clone(),
                },
                HttpHeader {
                    name: "x-aether-query".to_string(),
                    value: request.query.clone(),
                },
                HttpHeader {
                    name: "x-aether-peer-addr".to_string(),
                    value: request.peer_addr.clone(),
                },
                HttpHeader {
                    name: "content-type".to_string(),
                    value: "text/plain".to_string(),
                },
            ];
            HttpServerResponse {
                status: 200,
                headers,
                body: request.body,
            }
        }
    }

    /// Always replies `200` with a fixed non-empty body, regardless of
    /// method — unlike [`EchoHttpHandler`] (which echoes the request
    /// body, empty for HEAD by definition and so unable to prove body
    /// suppression), this handler always has a body to suppress.
    pub struct FixedBodyHttpHandler;

    /// Empty runtime state for the stateless fixed-body handler (ADR-0122).
    pub struct FixedBodyHttpHandlerState;

    #[actor(singleton)]
    impl NativeActor for FixedBodyHttpHandler {
        type State = FixedBodyHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_fixed_body_handler";

        fn init(
            (): (),
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<FixedBodyHttpHandlerState, BootError> {
            Ok(FixedBodyHttpHandlerState)
        }

        #[handler]
        fn on_request(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpServerResponse {
            HttpServerResponse {
                status: 200,
                headers: vec![HttpHeader {
                    name: "content-type".to_string(),
                    value: "text/plain".to_string(),
                }],
                body: b"fixed body".to_vec(),
            }
        }
    }

    /// Receives the request and returns without replying — the response-less
    /// chain the `502` settlement safety net covers.
    pub struct SilentHttpHandler;

    /// Empty runtime state for the stateless silent handler (ADR-0122).
    pub struct SilentHttpHandlerState;

    #[actor(singleton)]
    impl NativeActor for SilentHttpHandler {
        type State = SilentHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_silent_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<SilentHttpHandlerState, BootError> {
            Ok(SilentHttpHandlerState)
        }

        #[handler]
        fn on_request(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) {
            // Intentionally drops the request without replying.
        }
    }

    /// The number of body chunks [`StreamHttpHandler`] emits. Chosen well
    /// above the test's credit window so the round trip exercises credit
    /// replenishment across many refills, not just the initial grant.
    pub const STREAM_CHUNK_COUNT: u32 = 40;

    /// The bytes of chunk `index`: its zero-padded index, so the reassembled
    /// body is the deterministic concatenation `"000001…039"` a test can
    /// rebuild and compare against.
    pub fn stream_chunk_body(index: u32) -> Vec<u8> {
        format!("{index:03}").into_bytes()
    }

    /// A well-behaved response-streaming handler (ADR-0128): replies
    /// `HttpResponseStreamOpen`, then emits [`STREAM_CHUNK_COUNT`] chunks
    /// paced strictly against the credit it is granted, and terminates with
    /// `HttpResponseStreamEnd`.
    pub struct StreamHttpHandler;

    /// Per-stream progress for [`StreamHttpHandler`].
    pub struct StreamHttpHandlerState {
        stream_id: u64,
        next_index: u32,
        ended: bool,
    }

    #[actor(singleton)]
    impl NativeActor for StreamHttpHandler {
        type State = StreamHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_stream_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<StreamHttpHandlerState, BootError> {
            Ok(StreamHttpHandlerState {
                stream_id: 0,
                next_index: 0,
                ended: false,
            })
        }

        #[handler]
        fn on_request(
            state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpResponseStreamOpen {
            state.next_index = 0;
            state.ended = false;
            HttpResponseStreamOpen {
                status: 200,
                headers: vec![HttpHeader {
                    name: "content-type".to_string(),
                    value: "text/plain".to_string(),
                }],
            }
        }

        /// Spend the granted credit: send up to `credit.credit` more chunks,
        /// then terminate once all [`STREAM_CHUNK_COUNT`] have gone out.
        #[handler]
        fn on_credit(state: &mut Self::State, ctx: &mut NativeCtx<'_>, credit: HttpStreamCredit) {
            state.stream_id = credit.stream_id;
            let mut budget = credit.credit;
            while budget > 0 && state.next_index < STREAM_CHUNK_COUNT {
                ctx.actor::<HttpServerCapability>()
                    .send(&HttpResponseChunk {
                        stream_id: state.stream_id,
                        body: stream_chunk_body(state.next_index),
                    });
                state.next_index += 1;
                budget -= 1;
            }
            if state.next_index >= STREAM_CHUNK_COUNT && !state.ended {
                ctx.actor::<HttpServerCapability>()
                    .send(&HttpResponseStreamEnd {
                        stream_id: state.stream_id,
                    });
                state.ended = true;
            }
        }
    }

    /// The number of chunks [`FloodHttpHandler`] blasts on its first credit,
    /// far more than any small test window — enough that the cap's credit
    /// accounting hits zero and the over-window guard tears the stream down.
    pub const FLOOD_CHUNK_COUNT: u32 = 200;

    /// A misbehaving response-streaming handler (ADR-0128 trust boundary):
    /// it replies `HttpResponseStreamOpen`, then on its first credit ignores
    /// the granted amount entirely and floods [`FLOOD_CHUNK_COUNT`] chunks.
    pub struct FloodHttpHandler;

    /// Guards [`FloodHttpHandler`] against re-flooding on replenishment
    /// credit (which never arrives once the cap tears the stream down).
    pub struct FloodHttpHandlerState {
        flooded: bool,
    }

    #[actor(singleton)]
    impl NativeActor for FloodHttpHandler {
        type State = FloodHttpHandlerState;
        type Config = ();
        const NAMESPACE: &'static str = "aether.http.test_flood_handler";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<FloodHttpHandlerState, BootError> {
            Ok(FloodHttpHandlerState { flooded: false })
        }

        #[handler]
        fn on_request(
            _state: &mut Self::State,
            _ctx: &mut NativeCtx<'_>,
            _request: HttpServerRequest,
        ) -> HttpResponseStreamOpen {
            HttpResponseStreamOpen {
                status: 200,
                headers: Vec::new(),
            }
        }

        #[handler]
        fn on_credit(state: &mut Self::State, ctx: &mut NativeCtx<'_>, credit: HttpStreamCredit) {
            if state.flooded {
                return;
            }
            state.flooded = true;
            for _ in 0..FLOOD_CHUNK_COUNT {
                ctx.actor::<HttpServerCapability>()
                    .send(&HttpResponseChunk {
                        stream_id: credit.stream_id,
                        body: vec![b'x'; 8],
                    });
            }
        }
    }
}

fn config_for(handler: &str, max_request_bytes: usize) -> HttpServerConfig {
    HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        handler_mailbox: handler.to_string(),
        max_request_bytes,
        request_timeout_millis: 5_000,
        ..HttpServerConfig::default()
    }
}

/// Server config for the streaming tests (ADR-0128): a small credit window
/// so a multi-chunk response must replenish credit repeatedly, and a flood
/// overruns it fast.
fn stream_config_for(handler: &str, window: u32) -> HttpServerConfig {
    HttpServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        handler_mailbox: handler.to_string(),
        request_timeout_millis: 5_000,
        response_stream_window: window,
        ..HttpServerConfig::default()
    }
}

/// Index of the first `\r\n` at or after `from`, or `None`.
fn find_crlf(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len().saturating_sub(1)).find(|&i| bytes[i] == b'\r' && bytes[i + 1] == b'\n')
}

/// Reassemble a chunked transfer-encoding body (everything after the head's
/// blank line) into its payload, stopping at the zero-length terminator.
fn dechunk(body: &str) -> String {
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

fn port_of(chassis: &PassiveChassis<TestChassis>) -> u16 {
    chassis
        .handle::<HttpServerHandle>()
        .expect("HttpServerHandle published")
        .local_port
}

/// Open a client `TcpStream` to the server's OS-picked port, write the
/// raw request, and read the full response (the cap sends
/// `Connection: close`, so the read terminates at EOF).
fn round_trip(port: u16, request: &[u8]) -> String {
    let mut stream =
        TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
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
    String::from_utf8_lossy(&response).into_owned()
}

/// The light non-contention test: the cap binds and publishes the bound
/// port.
#[test]
fn binds_and_publishes_port() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<HttpServerCapability>(config_for("aether.http.test_echo_handler", 1024))
        .build_passive()
        .expect("http server boots");
    assert!(port_of(&chassis) > 0, "bound to an OS-picked port");
}

fn body_of(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

/// A GET round-trips to the handler and its reply returns as
/// well-formed HTTP/1.1, carrying the parsed path / query / method.
#[test]
fn get_round_trips_to_handler() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(
            <EchoHttpHandler as Addressable>::NAMESPACE,
            1024,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"GET /hello?name=ada HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected 200 status line, got: {response:?}",
    );
    assert!(
        response.contains("x-aether-method: Get\r\n"),
        "{response:?}"
    );
    assert!(
        response.contains("x-aether-path: /hello\r\n"),
        "{response:?}"
    );
    assert!(
        response.contains("x-aether-query: name=ada\r\n"),
        "{response:?}",
    );
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
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(
            <EchoHttpHandler as Addressable>::NAMESPACE,
            1024,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected 200, got: {response:?}",
    );
    assert!(
        response.contains("x-aether-method: Post\r\n"),
        "{response:?}"
    );
    assert_eq!(body_of(&response), "hello", "body echoed verbatim");
}

/// An announced `Content-Length` past the body cap is answered
/// `413` before any dispatch.
#[test]
fn oversize_body_is_413() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(
            <EchoHttpHandler as Addressable>::NAMESPACE,
            8,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"POST /big HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 413 "),
        "expected 413, got: {response:?}",
    );
}

/// A non-enumerated method is answered `501` before any dispatch.
#[test]
fn unknown_method_is_501() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(
            <EchoHttpHandler as Addressable>::NAMESPACE,
            1024,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"FROB /x HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 501 "),
        "expected 501, got: {response:?}",
    );
}

/// A request whose configured handler resolves to nothing is
/// answered `503`.
#[test]
fn no_handler_is_503() {
    let (registry, mailer) = fresh_substrate();
    // The handler mailbox is named but no actor is registered under it.
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<HttpServerCapability>(config_for("aether.http.absent_handler", 1024))
        .build_passive()
        .expect("server boots");

    let response = round_trip(
        port_of(&chassis),
        b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 503 "),
        "expected 503, got: {response:?}",
    );
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
        .with_actor::<HttpServerCapability>(config_for(
            <SilentHttpHandler as Addressable>::NAMESPACE,
            1024,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"GET /drop HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 502 "),
        "expected 502, got: {response:?}",
    );
}

/// A percent-encoded path is decoded before it reaches the handler
/// (ADR-0108 §2's "the decoded path component"); the query string stays
/// raw.
#[test]
fn percent_encoded_path_is_decoded() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(
            <EchoHttpHandler as Addressable>::NAMESPACE,
            1024,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"GET /hello%20world?x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected 200, got: {response:?}",
    );
    assert!(
        response.contains("x-aether-path: /hello world\r\n"),
        "{response:?}",
    );
    assert!(response.contains("x-aether-query: x=1\r\n"), "{response:?}");
}

/// A `Transfer-Encoding` request head (chunked bodies are the parked
/// streaming surface, ADR-0108 §4) is rejected `411` rather than
/// dispatched with a silently-dropped body.
#[test]
fn transfer_encoding_is_411() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(
            <EchoHttpHandler as Addressable>::NAMESPACE,
            1024,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"POST /chunked HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 411 "),
        "expected 411, got: {response:?}",
    );
}

/// A request carrying `Expect: 100-continue` receives the interim `100
/// Continue` before the final response.
#[test]
fn expect_continue_gets_100_continue() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(
            <EchoHttpHandler as Addressable>::NAMESPACE,
            1024,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"POST /submit HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(
        response.starts_with("HTTP/1.1 100 Continue\r\n\r\n"),
        "expected interim 100 Continue, got: {response:?}",
    );
    assert!(
        response.contains("HTTP/1.1 200 OK\r\n"),
        "expected final 200 after the interim, got: {response:?}",
    );
}

/// A HEAD response carries the handler's headers — including the
/// `Content-Length` the body would have had — but no body bytes; a GET
/// to the same handler still returns the body.
#[test]
fn head_response_suppresses_body() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<FixedBodyHttpHandler>(())
        .with_actor::<HttpServerCapability>(config_for(
            <FixedBodyHttpHandler as Addressable>::NAMESPACE,
            1024,
        ))
        .build_passive()
        .expect("caps boot");
    let port = port_of(&chassis);

    let head_response = round_trip(port, b"HEAD /x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(
        head_response.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected 200, got: {head_response:?}",
    );
    assert!(
        head_response.contains("Content-Length: 10\r\n"),
        "{head_response:?}",
    );
    assert_eq!(
        body_of(&head_response),
        "",
        "HEAD must not carry a message body",
    );

    let get_response = round_trip(port, b"GET /x HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(body_of(&get_response), "fixed body");
}

/// A peer accepted past `max_connections` is refused a canned `503`
/// and closed before a reader thread is spawned; it never reaches the
/// handler.
///
/// Tripwire: without the capacity guard in `spawn_reader_for_peer`,
/// this connection is accepted and dispatched (or hangs waiting on
/// the handler) instead of being refused.
#[test]
fn over_capacity_connection_is_503() {
    let (registry, mailer) = fresh_substrate();
    let max_connections = 2;
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<HttpServerCapability>(HttpServerConfig {
            bind_addr: "127.0.0.1:0".to_string(),
            handler_mailbox: <EchoHttpHandler as Addressable>::NAMESPACE.to_string(),
            request_timeout_millis: 5_000,
            max_connections,
            ..HttpServerConfig::default()
        })
        .build_passive()
        .expect("caps boot");

    let port = port_of(&chassis);

    // Fill the connection table: each socket sends a partial request
    // head (no terminating blank line), so its reader thread blocks
    // waiting for more bytes and its `ConnState` stays resident.
    let mut held = Vec::new();
    for _ in 0..max_connections {
        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect to http server");
        stream
            .write_all(b"GET / HTTP/1.1\r\n")
            .expect("write partial request head");
        stream.flush().expect("flush partial request head");
        held.push(stream);
    }

    // Give the dispatcher a moment to drain the `PeerAccepted` events
    // into `connections` before the next connect.
    thread::sleep(Duration::from_millis(200));

    let response = round_trip(port, b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(
        response.starts_with("HTTP/1.1 503 "),
        "expected 503, got: {response:?}",
    );

    drop(held);
}

/// A streaming handler (ADR-0128) emits its body across more chunks than the
/// credit window, and the cap streams them as chunked transfer-encoding that
/// the client reassembles intact — exercising credit replenishment across
/// many refills, not just the initial grant.
#[test]
fn streamed_response_reassembles_across_credit_window() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<StreamHttpHandler>(())
        .with_actor::<HttpServerCapability>(stream_config_for(
            <StreamHttpHandler as Addressable>::NAMESPACE,
            // Window well below the chunk count so credit must replenish.
            8,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert!(
        response.contains("Transfer-Encoding: chunked\r\n"),
        "streamed response is chunked: {response:?}",
    );
    assert!(
        !response.contains("Content-Length:"),
        "streamed response omits Content-Length: {response:?}",
    );

    let expected: Vec<u8> = (0..STREAM_CHUNK_COUNT)
        .flat_map(stream_chunk_body)
        .collect();
    assert_eq!(
        dechunk(body_of(&response)).into_bytes(),
        expected,
        "reassembled body matches every emitted chunk in order",
    );
}

/// Tripwire: a handler that floods chunks past its granted credit
/// (ADR-0128 §Consequences trust boundary) is torn down by the cap — the
/// response head and some chunks arrive, but the stream never reaches its
/// terminating zero-length chunk, so a misbehaving producer cannot outrun
/// the window unbounded.
#[test]
fn over_window_flood_tears_the_stream_down() {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<FloodHttpHandler>(())
        .with_actor::<HttpServerCapability>(stream_config_for(
            <FloodHttpHandler as Addressable>::NAMESPACE,
            // Tiny window so the flood overruns credit within a few chunks.
            2,
        ))
        .build_passive()
        .expect("caps boot");

    let response = round_trip(
        port_of(&chassis),
        b"GET /flood HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
    assert!(
        response.contains("Transfer-Encoding: chunked\r\n"),
        "flood stream head is chunked before teardown: {response:?}",
    );
    assert!(
        !response.contains("0\r\n\r\n"),
        "flood stream is torn down before the terminator: {response:?}",
    );
}

/// Boot the server (first, so the routed handlers' `wire` registrations
/// find its mailbox live) with [`FixedBodyHttpHandler`] as the
/// `handler_mailbox` fallback, then the given routed handlers.
macro_rules! routed_chassis {
    ($($handler:ty),+ $(,)?) => {{
        let (registry, mailer) = fresh_substrate();
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<HttpServerCapability>(config_for(
                <FixedBodyHttpHandler as Addressable>::NAMESPACE,
                1024,
            ))
            .with_actor::<FixedBodyHttpHandler>(())
            $(.with_actor::<$handler>(()))+
            .build_passive()
            .expect("caps boot")
    }};
}

/// Poll `request` until its response body equals `expected` (bounded
/// deadline, house pattern — see `tcp/tests.rs` / the bundle
/// `http_serving.rs`). The `wire` route registrations are asynchronous
/// mail, so a route's *first* positive assertion must poll it live
/// rather than race the registration; assertions that depend on an
/// already-proven-live route can then be direct.
fn poll_body(port: u16, request: &[u8], expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = round_trip(port, request);
        if body_of(&response) == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected body {expected:?} within 10s; last response: {response:?}",
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// A `wire`-registered route dispatches as the registered kind — the
/// handler's typed `#[handler]` decodes the request-shaped payload
/// under the minted kind and echoes the path — and an unrouted path
/// falls back to the configured `handler_mailbox` (ADR-0130).
#[test]
fn routed_prefix_dispatches_as_registered_kind() {
    let chassis = routed_chassis!(ApiRouteHandler);
    let port = port_of(&chassis);

    poll_body(
        port,
        b"GET /api/widgets HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "api:/api/widgets",
    );

    let unrouted = round_trip(port, b"GET /other HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(
        body_of(&unrouted),
        "fixed body",
        "unrouted path falls back to handler_mailbox",
    );
}

/// Longest prefix wins among overlapping routes, matching stops at
/// segment boundaries (`/apiary` is not under `/api`), and an exact
/// prefix hit routes (ADR-0130).
#[test]
fn longest_prefix_wins_on_segment_boundaries() {
    let chassis = routed_chassis!(ApiRouteHandler, ApiV2Handler);
    let port = port_of(&chassis);

    // Prove both routes live before the precedence assertions.
    poll_body(
        port,
        b"GET /api/other HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "api:/api/other",
    );
    poll_body(
        port,
        b"GET /api/v2/x HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "api-v2",
    );

    let exact = round_trip(port, b"GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(body_of(&exact), "api:/api", "exact prefix hit routes");

    let boundary = round_trip(port, b"GET /apiary HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(
        body_of(&boundary),
        "fixed body",
        "/apiary is not under /api — segment-boundary match",
    );
}

/// A method-specific route beats a method-agnostic one at equal
/// prefix; other methods take the agnostic route (ADR-0130).
#[test]
fn method_specific_route_beats_agnostic() {
    let chassis = routed_chassis!(MethodPostHandler, MethodAnyHandler);
    let port = port_of(&chassis);

    // Each route's first positive assertion polls it live; together
    // they then pin the precedence (POST → specific, GET → agnostic).
    poll_body(
        port,
        b"POST /m HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        "post-m",
    );
    poll_body(port, b"GET /m HTTP/1.1\r\nHost: localhost\r\n\r\n", "any-m");

    let specific = round_trip(
        port,
        b"POST /m HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(
        body_of(&specific),
        "post-m",
        "POST takes the method-specific route with both routes live",
    );
}

/// A `(prefix, method)` key already claimed by another mailbox is
/// rejected: the first claimant keeps the route, and the rejected
/// claimant's other registrations are unaffected (ADR-0130). Boot
/// order makes the winner deterministic — `DupFirstHandler` wires
/// before `DupSecondHandler`.
#[test]
fn conflicting_claim_is_rejected_first_claimant_keeps_route() {
    let chassis = routed_chassis!(DupFirstHandler, DupSecondHandler);
    let port = port_of(&chassis);

    // `/second` live proves DupSecondHandler's registrations (sent
    // after DupFirstHandler's, in boot order) have been processed —
    // including its rejected `/dup` claim.
    poll_body(
        port,
        b"GET /second HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "second",
    );

    let dup = round_trip(port, b"GET /dup HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(body_of(&dup), "first", "first claimant keeps the route");
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
    poll_body(
        port,
        b"GET /tmp HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "fixed body",
    );
}
