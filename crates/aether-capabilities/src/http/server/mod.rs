//! `aether.http.server` — substrate HTTP server capability (ADR-0108,
//! issue 1760).
//!
//! Singleton actor modeled on [`RpcServerCapability`]. It binds a
//! `TcpListener` on the configured address at init, runs a sidecar accept
//! thread that hands each accepted socket to a per-connection reader
//! thread. A reader parses one HTTP/1.1 request (request line + headers +
//! a `Content-Length`-bounded body), pushes it over an internal mpsc, and
//! fires an [`HttpInboundReady`] wake mail at the cap's own mailbox so the
//! dispatcher drains the queue.
//!
//! On a parsed request the cap dispatches an
//! [`HttpServerRequest`](crate::http::kinds::HttpServerRequest) to the configured
//! handler mailbox as a fresh causal chain via
//! `NativeCtx::send_envelope_as_root` (the wake mail is causally unrelated
//! to the inbound request), records the open response socket in an
//! in-flight table keyed by the dispatch's correlation id, and subscribes
//! to settlement of the dispatched root. The handler replies
//! [`HttpServerResponse`]; the reply
//! routes back to the cap, the
//! reply-interception fallback formats the HTTP/1.1 response and writes it
//! to the held socket. A response-less chain settles into `502`, a
//! per-request timeout into `504`, and the trust caps reject oversize or
//! malformed input with `413` / `431` / `501` before any dispatch.
//!
//! ADR-0122 identity/runtime split: the addressing identity is the ZST
//! [`HttpServerCapability`]; the state-bearing runtime (the listener, the
//! accept thread, the connection table) lives in the `runtime` module behind the
//! one `feature = "runtime"` gate.
//!
//! [`RpcServerCapability`]: crate::rpc::RpcServerCapability

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds resolve at file root through these imports —
// `#[actor]` emits the `HandlesKind<K>` markers always-on against the
// identity, and the `init` / handler bodies name these kinds.
use crate::http::kinds::HttpInboundReady;
use aether_kinds::trace::Settled;

// Default bind address. Loopback per ADR-0108 §6 — binding a public
// interface is an explicit operator choice.
/// Default `bind_addr` when unset: loopback, OS-assigned port (ADR-0108 §6).
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";
/// Default `max_request_bytes` (request body cap): 1 `MiB`.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1_048_576;
/// Default `max_header_bytes` (request line + headers cap): 64 `KiB`.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 65_536;
/// Default `request_timeout_millis` (slow-loris read + response deadline): 30 s.
pub const DEFAULT_REQUEST_TIMEOUT_MILLIS: u64 = 30_000;

mod config;

pub use config::HttpServerConfig;
// The `Config` derive on `HttpServerConfig` emits these native-only sibling
// types in `config`; chassis CLI / boot wiring addresses them through the
// `server::` path, so re-export them here.
#[cfg(feature = "native")]
pub use config::{HttpServerConfigLayer, HttpServerOverlay};

/// Exported handle bundle published at boot. Reachable from the chassis
/// via `PassiveChassis::handle::<HttpServerHandle>()`; the load-bearing
/// field is `local_port` so embedders / tests can connect to the
/// OS-picked port when `bind_addr` requested port 0.
///
/// Plain data (no substrate type), so it stays at file root under the
/// existing `not(target_arch = "wasm32")` gate — the `pub use
/// server::HttpServerHandle` chain in `http/mod.rs` reads it from here.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct HttpServerHandle {
    pub local_port: u16,
}

/// `aether.http.server` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable`, the per-handler
/// `HandlesKind` markers, the `#[fallback]` reply-interception marker, and the
/// name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`HttpServerCapabilityState`, which owns the
/// listener + accept thread + connection table) lives behind the one
/// `feature = "runtime"` gate, so a transport-only build never names the state
/// type nor pulls `aether_substrate` through this cap.
pub struct HttpServerCapability;

use aether_actor::actor;

// The runtime half — the whole `aether_substrate`-typed surface (the state,
// the sidecar threads, the parse/render machinery) — lives in `runtime.rs`,
// gated once here; the `#[actor] impl` reaches it through the `use runtime::*`
// glob.
#[cfg(feature = "runtime")]
mod runtime;

#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

#[actor(singleton)]
impl NativeActor for HttpServerCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// listener port, the accept thread, the connection table, and the
    /// in-flight correlation table.
    type State = HttpServerCapabilityState;

    type Config = HttpServerConfig;

    const NAMESPACE: &'static str = "aether.http.server";

    fn init(
        config: HttpServerConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<HttpServerCapabilityState, BootError> {
        let listener =
            TcpListener::bind(&config.bind_addr).map_err(|e| BootError::Other(Box::new(e)))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| BootError::Other(Box::new(e)))?;
        let port = local_addr.port();
        listener
            .set_nonblocking(false)
            .map_err(|e| BootError::Other(Box::new(e)))?;

        let accept_shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown_for_thread = Arc::clone(&accept_shutdown);

        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
        let mailer: Arc<Mailer> = ctx.mailer();
        let self_id = ctx.self_id();
        let wake_kind = KindId(<HttpInboundReady as Kind>::ID.0);

        let accept_sink = WakeSink {
            inbound_tx: inbound_tx.clone(),
            mailer: Arc::clone(&mailer),
            self_id,
            wake_kind,
        };

        // Transport thread below the mail layer — it accepts sockets
        // that carry inbound mail in; no inbound chain to inherit, no
        // settlement umbrella.
        #[allow(clippy::disallowed_methods)]
        let accept_thread = thread::Builder::new()
            .name(format!("aether-http-accept-{port}"))
            .spawn(move || {
                while !accept_shutdown_for_thread.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            if accept_shutdown_for_thread.load(Ordering::Acquire) {
                                drop(stream);
                                break;
                            }
                            if !accept_sink.post(InboundEvent::PeerAccepted { stream, peer }) {
                                break;
                            }
                        }
                        Err(_) => {
                            if accept_shutdown_for_thread.load(Ordering::Acquire) {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        tracing::info!(
            target: "aether_substrate::http_server",
            addr = %config.bind_addr,
            port,
            handler = %config.handler_mailbox,
            "http server bound",
        );

        ctx.publish_handle(HttpServerHandle { local_port: port });

        Ok(HttpServerCapabilityState {
            handler_mailbox: config.handler_mailbox,
            max_request_bytes: config.max_request_bytes,
            max_header_bytes: config.max_header_bytes,
            request_timeout: Duration::from_millis(config.request_timeout_millis),
            self_mailbox: self_id,
            mailer,
            listener_port: port,
            accept_shutdown,
            accept_thread: Some(accept_thread),
            inbound_rx,
            inbound_tx,
            connections: HashMap::new(),
            next_conn_id: 0,
            in_flight: HashMap::new(),
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // Stop the accept thread; self-connect to unblock its
        // blocking `accept()`.
        state.accept_shutdown.store(true, Ordering::Release);
        if let Ok(addr) = format!("127.0.0.1:{}", state.listener_port).parse::<SocketAddr>() {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
        }
        if let Some(thread) = state.accept_thread.take() {
            let _ = thread.join();
        }
        // Stop every per-connection reader. Shutting the socket down
        // wakes the blocked `read()`; the reader sees the flag and exits.
        for conn in state.connections.values_mut() {
            conn.shutdown.store(true, Ordering::Release);
            let _ = conn.write_half.shutdown(Shutdown::Both);
            if let Some(thread) = conn.reader_thread.take() {
                let _ = thread.join();
            }
        }
        tracing::info!(
            target: "aether_substrate::http_server",
            port = state.listener_port,
            "http server closed",
        );
    }

    /// Sidecar wake. Drain every pending inbound event.
    ///
    /// # Agent
    /// Internal wake mail — not part of the cap's external surface. The
    /// accept / reader sidecars fire this; the handler drains the mpsc
    /// and acts per item.
    #[handler]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: HttpInboundReady) {
        while let Ok(event) = state.inbound_rx.try_recv() {
            match event {
                InboundEvent::PeerAccepted { stream, peer } => {
                    state.spawn_reader_for_peer(stream, peer);
                }
                InboundEvent::RequestParsed { conn_id, request } => {
                    state.dispatch_request(ctx, conn_id, request);
                }
                InboundEvent::RequestRejected {
                    conn_id,
                    status,
                    message,
                } => {
                    state.write_status_response(conn_id, status, message);
                    state.close_connection(conn_id, "request rejected");
                }
                InboundEvent::ReaderClosed { conn_id, reason } => {
                    state.close_connection(conn_id, &reason);
                }
                InboundEvent::RequestTimedOut { conn_id } => {
                    if state.in_flight.values().any(|p| p.conn_id == conn_id) {
                        state.write_status_response(conn_id, 504, "gateway timeout");
                    }
                    state.close_connection(conn_id, "request timeout");
                }
            }
        }
    }

    /// Settlement notice. The root corresponds to a dispatched request
    /// we subscribed to; if it settled with no [`HttpServerResponse`]
    /// written, answer `502` (ADR-0108 §5) and clear the entry.
    ///
    /// # Agent
    /// Internal — fires from the settlement registry, not external mail.
    #[handler]
    fn on_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Settled) {
        let correlation = mail.root.correlation_id;
        let Some(pending) = state.in_flight.remove(&correlation) else {
            // Already answered (the reply landed first) or never ours.
            return;
        };
        state.write_status_response(pending.conn_id, 502, "no response from handler");
        state.close_connection(pending.conn_id, "settled without response");
    }

    /// Reply interception. Any mail addressed at this cap that isn't one
    /// of the typed wake / settlement kinds is treated as the handler's
    /// reply; if its `correlation_id` matches an in-flight request and
    /// it is an [`HttpServerResponse`], format the HTTP/1.1 response,
    /// write it to the held socket, and close.
    ///
    /// # Agent
    /// Not user-callable — this is the cap's reply-interception path. A
    /// by-value `#[handler]` can't read the inbound `sender.correlation_id`,
    /// so reply correlation goes through this envelope fallback
    /// (ADR-0108 §5).
    #[fallback]
    fn on_any(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
        let correlation = env.sender.correlation_id;
        let Some(pending) = state.in_flight.get(&correlation).copied() else {
            return;
        };
        if env.kind != <HttpServerResponse as Kind>::ID {
            // Unexpected kind with a matching correlation — leave the
            // in-flight entry for the settlement / timeout safety net.
            return;
        }
        match HttpServerResponse::decode_from_bytes(env.payload.bytes()) {
            Some(response) => state.write_handler_response(pending.conn_id, &response),
            None => {
                state.write_status_response(pending.conn_id, 502, "malformed handler response");
            }
        }
        state.in_flight.remove(&correlation);
        state.close_connection(pending.conn_id, "response written");
    }
}

#[cfg(all(test, feature = "runtime"))]
mod test_handlers {
    //! Minimal native handler actors behind the server in the integration
    //! tests: one that replies `200` echoing the request, one that drops
    //! the request without replying (the `502` safety-net path).
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    use crate::http::kinds::{HttpHeader, HttpServerRequest, HttpServerResponse};

    /// Replies `200` and echoes the request's method / path / query (as
    /// headers) and body (verbatim), so a test can assert the full request
    /// round-tripped to the handler.
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
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::test_handlers::{EchoHttpHandler, SilentHttpHandler};
    use super::{HttpServerCapability, HttpServerConfig, HttpServerHandle};
    use crate::test_chassis::{TestChassis, fresh_substrate};
    use crate::trace::TraceDispatchCapability;
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use std::io::{self, Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    fn config_for(handler: &str, max_request_bytes: usize) -> HttpServerConfig {
        HttpServerConfig {
            bind_addr: "127.0.0.1:0".to_string(),
            handler_mailbox: handler.to_string(),
            max_request_bytes,
            request_timeout_millis: 5_000,
            ..HttpServerConfig::default()
        }
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
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
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

    use aether_actor::Addressable;

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
}
