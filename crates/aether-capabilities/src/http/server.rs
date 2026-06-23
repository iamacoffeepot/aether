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
//! [`HttpServerResponse`](crate::http::kinds::HttpServerResponse); the reply
//! routes back to the cap, the
//! reply-interception fallback formats the HTTP/1.1 response and writes it
//! to the held socket. A response-less chain settles into `502`, a
//! per-request timeout into `504`, and the trust caps reject oversize or
//! malformed input with `413` / `431` / `501` before any dispatch.
//!
//! [`RpcServerCapability`]: crate::rpc::RpcServerCapability

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds need to be importable at file root for the
// `#[bridge]`-emitted `HandlesKind<K>` markers.
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

// Re-export the cap's handle struct at file root for chassis builders +
// embedders that read the bound port. The native-only `*Layer` /
// `*Overlay` the `Config` derive emits live next to `HttpServerConfig`.
#[cfg(not(target_arch = "wasm32"))]
pub use server_native::HttpServerHandle;

/// Init config for [`HttpServerCapability`] (ADR-0108).
///
/// `bind_addr` is the address to bind (e.g. `"127.0.0.1:8080"`,
/// `"127.0.0.1:0"` to let the OS pick a port). `handler_mailbox` names the
/// single component mailbox every request is dispatched to — resolved by
/// name at dispatch time (late binding), so the handler component can load
/// or reload independently of the server. `max_request_bytes` caps the
/// request body, `max_header_bytes` caps the request line + headers, and
/// `request_timeout_millis` bounds both the per-read slow-loris timeout and
/// the handler response deadline.
///
/// `#[derive(aether_substrate::Config)]` (ADR-0090) emits the env-shaped
/// `HttpServerConfigLayer`, the clap-shaped `HttpServerOverlay`, and the
/// inherent `from_env` / `from_argv_then_env` shims under
/// `feature = "native"`; the wasm-marker build carries only this domain
/// struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "native", derive(aether_substrate::Config))]
#[cfg_attr(
    feature = "native",
    config(env_prefix = "AETHER_HTTP_SERVER", cli_prefix = "http-server")
)]
pub struct HttpServerConfig {
    /// Whether to bind the listening socket at all. Default `false` —
    /// the HTTP server is opt-in, so an unconfigured chassis binds no
    /// port. The remaining fields are consulted only when this is `true`.
    #[cfg_attr(feature = "native", config(default = false))]
    pub enabled: bool,
    /// Address to bind the listening socket. Defaults to loopback
    /// ([`DEFAULT_BIND_ADDR`]); a public interface is an explicit choice.
    /// A blank override (`AETHER_HTTP_SERVER_BIND_ADDR=`) falls back to
    /// the default — the derive treats an empty `String` as unset.
    #[cfg_attr(feature = "native", config(default = "127.0.0.1:8080"))]
    pub bind_addr: String,
    /// The single handler mailbox every request is dispatched to (e.g.
    /// `"aether.component/aether.embedded:web"`). Empty = every request is
    /// answered `503` (no handler resolves).
    #[cfg_attr(feature = "native", config(default = ""))]
    pub handler_mailbox: String,
    /// Cap on the request body in bytes ([`DEFAULT_MAX_REQUEST_BYTES`]);
    /// an announced `Content-Length` past this is answered `413`.
    #[cfg_attr(feature = "native", config(default = 1_048_576))]
    pub max_request_bytes: usize,
    /// Cap on the request line + header bytes ([`DEFAULT_MAX_HEADER_BYTES`]);
    /// a head that grows past this is answered `431`.
    #[cfg_attr(feature = "native", config(default = 65_536))]
    pub max_header_bytes: usize,
    /// Per-read socket timeout (slow-loris guard) and handler response
    /// deadline in milliseconds ([`DEFAULT_REQUEST_TIMEOUT_MILLIS`]); a
    /// handler that doesn't reply in time yields `504`.
    #[cfg_attr(feature = "native", config(default = 30_000))]
    pub request_timeout_millis: u64,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            handler_mailbox: String::new(),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            request_timeout_millis: DEFAULT_REQUEST_TIMEOUT_MILLIS,
        }
    }
}

#[aether_actor::bridge(singleton)]
mod server_native {
    use super::{HttpInboundReady, Settled};
    use crate::http::kinds::{HttpHeader, HttpMethod, HttpServerRequest, HttpServerResponse};
    use aether_actor::actor;
    use aether_data::{Kind, KindId, MailboxId};
    use aether_substrate::Mail;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::mailer::Mailer;
    use std::collections::HashMap;
    use std::io::{self, Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Per-connection identifier, monotonic within this cap. Distinct from
    /// the OS-level peer addr (one peer may reconnect; ids stay unique for
    /// the cap's lifetime).
    type ConnId = u64;

    /// Header-array size for the inbound parse (doubles as the header-count
    /// cap: a request with more headers is answered `431`). ADR-0108 §6.
    const MAX_HEADER_COUNT: usize = 64;

    /// One parsed inbound HTTP/1.1 request the reader hands to the
    /// dispatcher. The method stays a raw `String` here; the dispatcher
    /// maps it to [`HttpMethod`] and answers `501` for a non-enumerated
    /// verb before any dispatch.
    struct ParsedRequest {
        method: String,
        path: String,
        query: String,
        headers: Vec<HttpHeader>,
        body: Vec<u8>,
    }

    /// Internal event the accept / reader sidecar threads push to the cap
    /// dispatcher via an mpsc. The matching wake-mail kind is
    /// [`HttpInboundReady`] (empty payload) — `on_inbound_ready` drains the
    /// channel and acts per item.
    enum InboundEvent {
        /// The accept thread took a new connection.
        PeerAccepted { stream: TcpStream, peer: SocketAddr },
        /// A reader parsed a complete, size-bounded request.
        RequestParsed {
            conn_id: ConnId,
            request: ParsedRequest,
        },
        /// A reader hit a trust cap or a parse error before any dispatch;
        /// the dispatcher writes the canned status response and closes.
        RequestRejected {
            conn_id: ConnId,
            status: u16,
            message: &'static str,
        },
        /// A reader saw EOF / a read error / a slow-loris timeout; the
        /// dispatcher tears the connection down.
        ReaderClosed { conn_id: ConnId, reason: String },
        /// The handler didn't reply within `request_timeout`; the
        /// dispatcher writes `504` if the request is still in-flight.
        RequestTimedOut { conn_id: ConnId },
    }

    /// Per-connection state owned by the cap dispatcher. The reader sidecar
    /// holds `shutdown` + the read half; the dispatcher writes the response
    /// through `write_half`.
    struct ConnState {
        peer: SocketAddr,
        /// Dispatcher's half — used to write the HTTP/1.1 response.
        write_half: TcpStream,
        /// Reader thread's shutdown flag. Cap flips it + shuts down the
        /// socket to wake the blocked `read()`.
        shutdown: Arc<AtomicBool>,
        /// Reader thread handle. Joined in `unwire`, detached on close.
        reader_thread: Option<JoinHandle<()>>,
    }

    /// Bookkeeping for one in-flight request. Looked up by the dispatch's
    /// auto-minted `correlation_id` (== the dispatched envelope's
    /// `MailId.correlation_id`, which is also the root id since the cap
    /// always dispatches via `send_envelope_as_root`).
    #[derive(Copy, Clone)]
    struct PendingRequest {
        conn_id: ConnId,
    }

    /// Exported handle bundle published at boot. Reachable from the chassis
    /// via `PassiveChassis::handle::<HttpServerHandle>()`; the load-bearing
    /// field is `local_port` so embedders / tests can connect to the
    /// OS-picked port when `bind_addr` requested port 0.
    #[derive(Clone)]
    pub struct HttpServerHandle {
        pub local_port: u16,
    }

    /// Wake sink shared with the accept + reader sidecar threads: push an
    /// [`InboundEvent`] over the mpsc, then fire an [`HttpInboundReady`]
    /// wake mail at the cap so the dispatcher drains.
    struct WakeSink {
        inbound_tx: mpsc::Sender<InboundEvent>,
        mailer: Arc<Mailer>,
        self_id: MailboxId,
        wake_kind: KindId,
    }

    impl WakeSink {
        /// Post one event + wake. Returns `false` when the receiver is gone
        /// (the cap tore down) so the caller stops.
        fn post(&self, event: InboundEvent) -> bool {
            if self.inbound_tx.send(event).is_err() {
                return false;
            }
            self.mailer.push(Mail::new(
                self.self_id,
                self.wake_kind,
                HttpInboundReady::default().encode_into_bytes(),
                1,
            ));
            true
        }
    }

    /// Singleton HTTP server cap. Owns one TCP listener + per-connection
    /// state + the in-flight correlation table.
    pub struct HttpServerCapability {
        handler_mailbox: String,
        max_request_bytes: usize,
        max_header_bytes: usize,
        request_timeout: Duration,
        self_mailbox: MailboxId,
        /// Cached `Arc<Mailer>` so the dispatcher can fire wake mails into
        /// the cap, resolve the handler mailbox by name at dispatch time,
        /// and subscribe to settlement. The cap is single-threaded
        /// post-ADR-0038 so direct storage is fine.
        mailer: Arc<Mailer>,
        listener_port: u16,
        accept_shutdown: Arc<AtomicBool>,
        accept_thread: Option<JoinHandle<()>>,
        inbound_rx: mpsc::Receiver<InboundEvent>,
        inbound_tx: mpsc::Sender<InboundEvent>,
        connections: HashMap<ConnId, ConnState>,
        next_conn_id: ConnId,
        /// Dispatch-correlation → open response socket. Populated on
        /// dispatch; cleared on reply, settlement, timeout, or close.
        in_flight: HashMap<u64, PendingRequest>,
    }

    #[actor]
    impl NativeActor for HttpServerCapability {
        type Config = super::HttpServerConfig;
        const NAMESPACE: &'static str = "aether.http.server";

        fn init(
            config: super::HttpServerConfig,
            ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
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

            Ok(Self {
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

        fn unwire(&mut self, _ctx: &mut NativeCtx<'_>) {
            // Stop the accept thread; self-connect to unblock its
            // blocking `accept()`.
            self.accept_shutdown.store(true, Ordering::Release);
            if let Ok(addr) = format!("127.0.0.1:{}", self.listener_port).parse::<SocketAddr>() {
                let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
            }
            if let Some(thread) = self.accept_thread.take() {
                let _ = thread.join();
            }
            // Stop every per-connection reader. Shutting the socket down
            // wakes the blocked `read()`; the reader sees the flag and exits.
            for conn in self.connections.values_mut() {
                conn.shutdown.store(true, Ordering::Release);
                let _ = conn.write_half.shutdown(Shutdown::Both);
                if let Some(thread) = conn.reader_thread.take() {
                    let _ = thread.join();
                }
            }
            tracing::info!(
                target: "aether_substrate::http_server",
                port = self.listener_port,
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
        fn on_inbound_ready(&mut self, ctx: &mut NativeCtx<'_>, _mail: HttpInboundReady) {
            while let Ok(event) = self.inbound_rx.try_recv() {
                match event {
                    InboundEvent::PeerAccepted { stream, peer } => {
                        self.spawn_reader_for_peer(stream, peer);
                    }
                    InboundEvent::RequestParsed { conn_id, request } => {
                        self.dispatch_request(ctx, conn_id, request);
                    }
                    InboundEvent::RequestRejected {
                        conn_id,
                        status,
                        message,
                    } => {
                        self.write_status_response(conn_id, status, message);
                        self.close_connection(conn_id, "request rejected");
                    }
                    InboundEvent::ReaderClosed { conn_id, reason } => {
                        self.close_connection(conn_id, &reason);
                    }
                    InboundEvent::RequestTimedOut { conn_id } => {
                        if self.in_flight.values().any(|p| p.conn_id == conn_id) {
                            self.write_status_response(conn_id, 504, "gateway timeout");
                        }
                        self.close_connection(conn_id, "request timeout");
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
        fn on_settled(&mut self, _ctx: &mut NativeCtx<'_>, mail: Settled) {
            let correlation = mail.root.correlation_id;
            let Some(pending) = self.in_flight.remove(&correlation) else {
                // Already answered (the reply landed first) or never ours.
                return;
            };
            self.write_status_response(pending.conn_id, 502, "no response from handler");
            self.close_connection(pending.conn_id, "settled without response");
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
        fn on_any(&mut self, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
            let correlation = env.sender.correlation_id;
            let Some(pending) = self.in_flight.get(&correlation).copied() else {
                return;
            };
            if env.kind != <HttpServerResponse as Kind>::ID {
                // Unexpected kind with a matching correlation — leave the
                // in-flight entry for the settlement / timeout safety net.
                return;
            }
            match HttpServerResponse::decode_from_bytes(env.payload.bytes()) {
                Some(response) => self.write_handler_response(pending.conn_id, &response),
                None => {
                    self.write_status_response(pending.conn_id, 502, "malformed handler response");
                }
            }
            self.in_flight.remove(&correlation);
            self.close_connection(pending.conn_id, "response written");
        }
    }

    impl HttpServerCapability {
        /// Allocate a fresh `ConnId`, store the connection's write half, and
        /// spin a reader thread for the read half.
        fn spawn_reader_for_peer(&mut self, stream: TcpStream, peer: SocketAddr) {
            let conn_id = self.next_conn_id;
            self.next_conn_id += 1;

            let read_half = match stream.try_clone() {
                Ok(half) => half,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::http_server",
                        %peer,
                        error = %e,
                        "http conn: try_clone failed; dropping",
                    );
                    return;
                }
            };
            // Slow-loris guard + response deadline (ADR-0108 §6): bound
            // every blocking read on this socket.
            if let Err(e) = read_half.set_read_timeout(Some(self.request_timeout)) {
                tracing::warn!(
                    target: "aether_substrate::http_server",
                    %peer,
                    error = %e,
                    "http conn: set_read_timeout failed; dropping",
                );
                return;
            }
            let write_half = stream;
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_for_thread = Arc::clone(&shutdown);

            let sink = WakeSink {
                inbound_tx: self.inbound_tx.clone(),
                mailer: Arc::clone(&self.mailer),
                self_id: self.self_mailbox,
                wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
            };
            let max_request_bytes = self.max_request_bytes;
            let max_header_bytes = self.max_header_bytes;

            // Per-connection transport reader below the mail layer — carries
            // inbound mail in; no inbound chain to inherit, no settlement
            // umbrella.
            #[allow(clippy::disallowed_methods)]
            let thread = match thread::Builder::new()
                .name(format!("aether-http-reader-{conn_id}"))
                .spawn(move || {
                    run_reader_loop(
                        read_half,
                        conn_id,
                        &shutdown_for_thread,
                        &sink,
                        max_request_bytes,
                        max_header_bytes,
                    );
                }) {
                Ok(thread) => thread,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::http_server",
                        %peer,
                        error = %e,
                        "http reader thread spawn failed",
                    );
                    return;
                }
            };

            self.connections.insert(
                conn_id,
                ConnState {
                    peer,
                    write_half,
                    shutdown,
                    reader_thread: Some(thread),
                },
            );
            tracing::debug!(
                target: "aether_substrate::http_server",
                conn = conn_id,
                %peer,
                "http conn accepted",
            );
        }

        /// Map the method, resolve the handler, dispatch the request, and
        /// record the in-flight entry. Answers `501` / `503` inline.
        fn dispatch_request(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            conn_id: ConnId,
            request: ParsedRequest,
        ) {
            let Some(method) = parse_http_method(&request.method) else {
                self.write_status_response(conn_id, 501, "method not implemented");
                self.close_connection(conn_id, "unsupported method");
                return;
            };
            // Late-binding handler resolution (ADR-0108 §3): resolve the
            // configured mailbox by name at dispatch time through the
            // registry — the sanctioned runtime-name path, which folds a
            // lineage-rendered name to its id and reports a miss as `None`.
            // Nothing live under the name → `503`.
            let Some(handler) = self.mailer.registry().lookup(&self.handler_mailbox) else {
                self.write_status_response(conn_id, 503, "no handler registered");
                self.close_connection(conn_id, "handler unresolved");
                return;
            };
            let payload = HttpServerRequest {
                method,
                path: request.path,
                query: request.query,
                headers: request.headers,
                body: request.body,
            }
            .encode_into_bytes();
            let mail_id =
                ctx.send_envelope_as_root(handler, <HttpServerRequest as Kind>::ID, &payload);
            // Safety net (ADR-0108 §5): if the chain settles with no
            // response, `on_settled` answers `502`. Best-effort — a chassis
            // without the settlement registry still serves the reply path.
            if let Some(registry) = self.mailer.settlement_registry() {
                registry.subscribe_settlement_mail(
                    mail_id,
                    self.self_mailbox,
                    <Settled as Kind>::ID,
                    Arc::clone(&self.mailer),
                );
            }
            self.in_flight
                .insert(mail_id.correlation_id, PendingRequest { conn_id });
        }

        /// Format + write the handler's [`HttpServerResponse`].
        fn write_handler_response(&mut self, conn_id: ConnId, response: &HttpServerResponse) {
            let bytes = render_handler_response(response);
            self.write_raw_to(conn_id, &bytes);
        }

        /// Format + write a canned status response (the cap's own
        /// `413` / `431` / `501` / `502` / `503` / `504`).
        fn write_status_response(&mut self, conn_id: ConnId, status: u16, message: &str) {
            let bytes = render_status_response(status, message);
            self.write_raw_to(conn_id, &bytes);
        }

        fn write_raw_to(&mut self, conn_id: ConnId, bytes: &[u8]) {
            let Some(conn) = self.connections.get_mut(&conn_id) else {
                return;
            };
            if let Err(e) = conn
                .write_half
                .write_all(bytes)
                .and_then(|()| conn.write_half.flush())
            {
                tracing::debug!(
                    target: "aether_substrate::http_server",
                    conn = conn_id,
                    error = %e,
                    "http response write failed",
                );
            }
        }

        fn close_connection(&mut self, conn_id: ConnId, reason: &str) {
            let Some(mut conn) = self.connections.remove(&conn_id) else {
                return;
            };
            conn.shutdown.store(true, Ordering::Release);
            let _ = conn.write_half.shutdown(Shutdown::Both);
            // Detach the reader without joining inline — the dispatcher must
            // not block on it. The thread sees the shutdown (or its own EOF)
            // and exits; the JoinHandle drop detaches.
            drop(conn.reader_thread.take());
            // Drop any in-flight entry pinned to this connection so we don't
            // write to a dead socket.
            self.in_flight
                .retain(|_, pending| pending.conn_id != conn_id);
            tracing::debug!(
                target: "aether_substrate::http_server",
                conn = conn_id,
                peer = %conn.peer,
                reason,
                "http conn closed",
            );
        }
    }

    /// Outcome of [`read_more`].
    enum ReadStep {
        Filled(usize),
        Eof,
        Timeout,
        Error(String),
    }

    /// One bounded read off the socket, retrying past `Interrupted` and
    /// folding `WouldBlock` / `TimedOut` (the `set_read_timeout` expiry)
    /// into [`ReadStep::Timeout`].
    fn read_more(stream: &mut TcpStream, chunk: &mut [u8]) -> ReadStep {
        loop {
            match stream.read(chunk) {
                Ok(0) => return ReadStep::Eof,
                Ok(n) => return ReadStep::Filled(n),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    return ReadStep::Timeout;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return ReadStep::Error(format!("read error: {e}")),
            }
        }
    }

    /// One parsed request head.
    struct RequestHead {
        head_len: usize,
        method: String,
        path: String,
        query: String,
        headers: Vec<HttpHeader>,
        content_length: usize,
    }

    /// Outcome of [`parse_head`].
    enum HeadParse {
        Complete(RequestHead),
        NeedMore,
        Reject { status: u16, message: &'static str },
    }

    /// Parse the accumulated bytes as an HTTP/1.1 request head (request line
    /// + headers). Enforces the header-count cap (`431` via
    /// `TooManyHeaders`) and surfaces the header-byte cap to the caller via
    /// [`HeadParse::NeedMore`] (the caller rejects `431` once `buf` outgrows
    /// `max_header_bytes`).
    fn parse_head(buf: &[u8], max_header_bytes: usize) -> HeadParse {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADER_COUNT];
        let mut request = httparse::Request::new(&mut headers);
        match request.parse(buf) {
            Ok(httparse::Status::Complete(head_len)) => {
                let method = request.method.unwrap_or_default().to_string();
                let raw_path = request.path.unwrap_or("/");
                let (path, query) = match raw_path.split_once('?') {
                    Some((before, after)) => (before.to_string(), after.to_string()),
                    None => (raw_path.to_string(), String::new()),
                };
                let mut out_headers = Vec::with_capacity(request.headers.len());
                let mut content_length = 0usize;
                let mut bad_length = false;
                for header in &*request.headers {
                    let value = String::from_utf8_lossy(header.value).into_owned();
                    if header.name.eq_ignore_ascii_case("content-length") {
                        match value.trim().parse::<usize>() {
                            Ok(n) => content_length = n,
                            Err(_) => bad_length = true,
                        }
                    }
                    out_headers.push(HttpHeader {
                        name: header.name.to_string(),
                        value,
                    });
                }
                if bad_length {
                    return HeadParse::Reject {
                        status: 400,
                        message: "invalid content-length",
                    };
                }
                HeadParse::Complete(RequestHead {
                    head_len,
                    method,
                    path,
                    query,
                    headers: out_headers,
                    content_length,
                })
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() > max_header_bytes {
                    HeadParse::Reject {
                        status: 431,
                        message: "request header fields too large",
                    }
                } else {
                    HeadParse::NeedMore
                }
            }
            Err(httparse::Error::TooManyHeaders) => HeadParse::Reject {
                status: 431,
                message: "too many request headers",
            },
            Err(_) => HeadParse::Reject {
                status: 400,
                message: "malformed request",
            },
        }
    }

    /// Per-connection reader thread body. Reads one HTTP/1.1 request (head +
    /// `Content-Length`-bounded body), posts it, then blocks until the
    /// dispatcher writes the response and closes the socket (EOF) or the
    /// read timeout fires (`504`). Returns when the connection closes.
    #[allow(clippy::too_many_lines)]
    fn run_reader_loop(
        read_half: TcpStream,
        conn_id: ConnId,
        shutdown: &AtomicBool,
        sink: &WakeSink,
        max_request_bytes: usize,
        max_header_bytes: usize,
    ) {
        let mut stream = read_half;
        let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
        let mut chunk = [0u8; 8 * 1024];

        // Phase 1: accumulate the request head.
        let head = loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match parse_head(&buf, max_header_bytes) {
                HeadParse::Complete(head) => break head,
                HeadParse::Reject { status, message } => {
                    sink.post(InboundEvent::RequestRejected {
                        conn_id,
                        status,
                        message,
                    });
                    return;
                }
                HeadParse::NeedMore => {}
            }
            match read_more(&mut stream, &mut chunk) {
                ReadStep::Filled(n) => buf.extend_from_slice(&chunk[..n]),
                ReadStep::Eof => {
                    sink.post(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: "eof before request head".to_string(),
                    });
                    return;
                }
                ReadStep::Timeout => {
                    sink.post(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: "read timeout (head)".to_string(),
                    });
                    return;
                }
                ReadStep::Error(reason) => {
                    sink.post(InboundEvent::ReaderClosed { conn_id, reason });
                    return;
                }
            }
        };

        // Body cap (ADR-0108 §6): reject before reading any body bytes.
        if head.content_length > max_request_bytes {
            sink.post(InboundEvent::RequestRejected {
                conn_id,
                status: 413,
                message: "request body exceeds limit",
            });
            return;
        }

        // Phase 2: read the `Content-Length`-bounded body. Leftover bytes
        // already in `buf` past the head come first.
        let mut body: Vec<u8> = Vec::with_capacity(head.content_length);
        let leftover = &buf[head.head_len..];
        let take = leftover.len().min(head.content_length);
        body.extend_from_slice(&leftover[..take]);
        while body.len() < head.content_length {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match read_more(&mut stream, &mut chunk) {
                ReadStep::Filled(n) => {
                    let want = head.content_length - body.len();
                    body.extend_from_slice(&chunk[..n.min(want)]);
                }
                ReadStep::Eof => {
                    sink.post(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: "eof mid-body".to_string(),
                    });
                    return;
                }
                ReadStep::Timeout => {
                    sink.post(InboundEvent::ReaderClosed {
                        conn_id,
                        reason: "read timeout (body)".to_string(),
                    });
                    return;
                }
                ReadStep::Error(reason) => {
                    sink.post(InboundEvent::ReaderClosed { conn_id, reason });
                    return;
                }
            }
        }

        let request = ParsedRequest {
            method: head.method,
            path: head.path,
            query: head.query,
            headers: head.headers,
            body,
        };
        if !sink.post(InboundEvent::RequestParsed { conn_id, request }) {
            return;
        }

        // Phase 3: response deadline. Block until the dispatcher writes the
        // response and closes the socket (EOF) or the read timeout fires
        // (handler too slow → `504`). v1 serves one request per connection.
        loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match read_more(&mut stream, &mut chunk) {
                ReadStep::Eof | ReadStep::Error(_) => return,
                ReadStep::Filled(_) => {}
                ReadStep::Timeout => {
                    sink.post(InboundEvent::RequestTimedOut { conn_id });
                    return;
                }
            }
        }
    }

    /// Headers the cap supplies itself (ADR-0108 §2) — stripped from a
    /// handler's response so they aren't doubled.
    fn is_cap_owned_header(name: &str) -> bool {
        name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("date")
            || name.eq_ignore_ascii_case("transfer-encoding")
    }

    /// Render the handler's [`HttpServerResponse`] as an HTTP/1.1 response,
    /// supplying `Content-Length` / `Date` / `Connection` (ADR-0108 §2).
    fn render_handler_response(response: &HttpServerResponse) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut head = format!(
            "HTTP/1.1 {} {}\r\n",
            response.status,
            reason_phrase(response.status)
        );
        for header in &response.headers {
            if is_cap_owned_header(&header.name) {
                continue;
            }
            let _ = write!(head, "{}: {}\r\n", header.name, header.value);
        }
        let _ = write!(head, "Content-Length: {}\r\n", response.body.len());
        let _ = write!(head, "Date: {}\r\n", http_date(SystemTime::now()));
        head.push_str("Connection: close\r\n\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&response.body);
        out
    }

    /// Render a canned status response with a plain-text body.
    fn render_status_response(status: u16, message: &str) -> Vec<u8> {
        use std::fmt::Write as _;
        let body = message.as_bytes();
        let mut head = format!("HTTP/1.1 {} {}\r\n", status, reason_phrase(status));
        head.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        let _ = write!(head, "Content-Length: {}\r\n", body.len());
        let _ = write!(head, "Date: {}\r\n", http_date(SystemTime::now()));
        head.push_str("Connection: close\r\n\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// Map a raw HTTP method token to the typed [`HttpMethod`]; `None` for a
    /// non-enumerated verb (answered `501` before any dispatch).
    fn parse_http_method(method: &str) -> Option<HttpMethod> {
        match method {
            "GET" => Some(HttpMethod::Get),
            "POST" => Some(HttpMethod::Post),
            "PUT" => Some(HttpMethod::Put),
            "DELETE" => Some(HttpMethod::Delete),
            "PATCH" => Some(HttpMethod::Patch),
            "HEAD" => Some(HttpMethod::Head),
            "OPTIONS" => Some(HttpMethod::Options),
            _ => None,
        }
    }

    /// HTTP reason phrase for the status codes the cap emits.
    fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            404 => "Not Found",
            413 => "Payload Too Large",
            414 => "URI Too Long",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Status",
        }
    }

    /// Format `now` as an RFC 7231 IMF-fixdate (`Sun, 06 Nov 1994 08:49:37
    /// GMT`) for the `Date` response header. Pure integer arithmetic
    /// (Howard Hinnant's civil-from-days) — no date crate.
    fn http_date(now: SystemTime) -> String {
        const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let total = i64::try_from(secs).unwrap_or(i64::MAX);
        let days = total.div_euclid(86_400);
        let rem = total.rem_euclid(86_400);
        let hour = rem / 3_600;
        let minute = (rem % 3_600) / 60;
        let second = rem % 60;
        let weekday = (days + 4).rem_euclid(7);
        // Civil date from days-since-epoch.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if month <= 2 { year + 1 } else { year };
        let weekday_name = WEEKDAYS[usize::try_from(weekday).unwrap_or(0)];
        let month_name = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
        format!(
            "{weekday_name}, {day:02} {month_name} {year:04} {hour:02}:{minute:02}:{second:02} GMT"
        )
    }

    #[cfg(test)]
    mod unit_tests {
        use super::{http_date, parse_http_method, reason_phrase};
        use crate::http::kinds::HttpMethod;
        use std::time::{Duration, UNIX_EPOCH};

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

        #[test]
        fn reason_phrases_cover_emitted_statuses() {
            assert_eq!(reason_phrase(200), "OK");
            assert_eq!(reason_phrase(413), "Payload Too Large");
            assert_eq!(reason_phrase(501), "Not Implemented");
            assert_eq!(reason_phrase(502), "Bad Gateway");
            assert_eq!(reason_phrase(503), "Service Unavailable");
            assert_eq!(reason_phrase(504), "Gateway Timeout");
        }

        #[test]
        fn config_layer_defaults_match_the_named_consts() {
            use super::super::{
                DEFAULT_BIND_ADDR, DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_REQUEST_BYTES,
                DEFAULT_REQUEST_TIMEOUT_MILLIS, HttpServerConfig, HttpServerConfigLayer,
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
        }
    }
}

#[cfg(test)]
mod test_handlers {
    //! Minimal native handler actors behind the server in the integration
    //! tests: one that replies `200` echoing the request, one that drops
    //! the request without replying (the `502` safety-net path).
    use crate::http::kinds::{HttpHeader, HttpServerRequest, HttpServerResponse};

    /// Replies `200` and echoes the request's method / path / query (as
    /// headers) and body (verbatim), so a test can assert the full request
    /// round-tripped to the handler.
    #[aether_actor::bridge(singleton)]
    mod echo_handler {
        use super::{HttpHeader, HttpServerRequest, HttpServerResponse};
        use aether_actor::actor;
        use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
        use aether_substrate::chassis::error::BootError;

        pub struct EchoHttpHandler;

        #[actor]
        impl NativeActor for EchoHttpHandler {
            type Config = ();
            const NAMESPACE: &'static str = "aether.http.test_echo_handler";

            fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }

            #[allow(clippy::unused_self)]
            #[handler]
            fn on_request(
                &mut self,
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
    }

    /// Receives the request and returns without replying — the response-less
    /// chain the `502` settlement safety net covers.
    #[aether_actor::bridge(singleton)]
    mod silent_handler {
        use super::HttpServerRequest;
        use aether_actor::actor;
        use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
        use aether_substrate::chassis::error::BootError;

        pub struct SilentHttpHandler;

        #[actor]
        impl NativeActor for SilentHttpHandler {
            type Config = ();
            const NAMESPACE: &'static str = "aether.http.test_silent_handler";

            fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }

            #[allow(clippy::unused_self)]
            #[handler]
            fn on_request(&mut self, _ctx: &mut NativeCtx<'_>, _request: HttpServerRequest) {
                // Intentionally drops the request without replying.
            }
        }
    }
}

#[cfg(test)]
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
