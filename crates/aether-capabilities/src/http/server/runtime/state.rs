// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

use crate::http::server::shard::HttpDispatchShard;
use aether_substrate::Subname;

/// `aether.http.server` supervisor state (ADR-0135). Owns the TCP listener +
/// accept thread, the shared route table, the global live-connection ceiling,
/// and the dispatch-shard sinks. Per-request work never runs here — the
/// supervisor's steady-state job is assigning each accepted connection to a
/// shard and serving the route-registration surface (ADR-0130).
pub struct HttpSupervisorState {
    /// The resolved boot config, kept whole: the tuning fields seed each
    /// shard at spawn, and `max_connections` backs the assignment-time
    /// ceiling check.
    pub config: HttpServerConfig,
    /// Registered routes (ADR-0130), shared with every shard (and, in later
    /// stages, the readers): the supervisor's registration handlers write
    /// under the lock, dispatch-time resolution reads. Unordered —
    /// resolution picks the winner per request by `(prefix length, method
    /// specificity)`, which is deterministic without a sort: two distinct
    /// equal-length prefixes cannot both match one path, and duplicate
    /// `(prefix, method)` keys are rejected at registration. Route counts
    /// are tens per substrate, so the linear scan is dwarfed by the header
    /// parse that precedes it (ADR-0130).
    pub routes: SharedRoutes,
    /// Global live-connection count backing the `max_connections` ceiling
    /// (ADR-0108 §6): incremented here at assignment, decremented by the
    /// owning shard on connection close. An atomic rather than a table —
    /// the connections themselves live sharded.
    pub live_connections: Arc<AtomicUsize>,
    /// Cached `Arc<Mailer>` for registry validation in the registration
    /// handlers and for building each shard's wake sink.
    pub mailer: Arc<Mailer>,
    pub listener_port: u16,
    pub accept_shutdown: Arc<AtomicBool>,
    pub accept_thread: Option<JoinHandle<()>>,
    /// The supervisor's own sidecar channel: the accept thread posts
    /// [`InboundEvent::PeerAccepted`] here; nothing else feeds it.
    pub inbound_rx: mpsc::Receiver<InboundEvent>,
    /// The supervisor drain loop's wake-coalescing flag (ADR-0135 §4),
    /// shared with the accept sink; cleared at the top of
    /// `on_inbound_ready`.
    pub wake_dirty: Arc<AtomicBool>,
    /// One wake sink per spawned dispatch shard, in spawn order; empty until
    /// the first accepted connection forces the spawn (the dispatcher ctx is
    /// not available at `init`).
    pub shards: Vec<WakeSink>,
    /// Round-robin cursor over `shards`.
    pub next_shard: usize,
    /// Cap-global stream-id source, cloned into every shard's seed
    /// (ADR-0135) — see [`HttpShardState::next_stream_id`].
    pub next_stream_id: Arc<AtomicU64>,
}

/// Dispatch-shard state (ADR-0135): today's whole per-connection machine —
/// connection table, in-flight correlation table, response/request stream
/// tables, websocket state — over the 1/N slice of connections the
/// supervisor assigned here. The dispatcher holds this as the shard actor's
/// state; the addressing identity is the distinct ZST
/// [`HttpDispatchShard`](crate::http::server::shard::HttpDispatchShard).
pub struct HttpShardState {
    pub handler_mailbox: String,
    /// The supervisor's shared route table (ADR-0130/0135); this shard only
    /// reads it, at request-dispatch time.
    pub routes: SharedRoutes,
    /// The global live-connection count (ADR-0135): the supervisor
    /// increments at assignment; this shard decrements when it closes (or
    /// fails to adopt) one of its connections.
    pub live_connections: Arc<AtomicUsize>,
    pub max_request_bytes: usize,
    pub max_header_bytes: usize,
    pub request_timeout: Duration,
    /// Idle timeout between requests on a kept-alive connection (and for a
    /// fresh connection that never sends its first byte). Distinct from
    /// `request_timeout`, which stays the in-flight read + response
    /// deadline.
    pub keep_alive_timeout: Duration,
    pub self_mailbox: MailboxId,
    /// Cached `Arc<Mailer>` so the shard can fire wake mails into itself,
    /// resolve the handler mailbox by name at dispatch time, and subscribe
    /// to settlement. The shard is single-threaded post-ADR-0038 so direct
    /// storage is fine.
    pub mailer: Arc<Mailer>,
    pub inbound_rx: mpsc::Receiver<InboundEvent>,
    pub inbound_tx: mpsc::Sender<InboundEvent>,
    /// This shard's wake-coalescing flag (ADR-0135 §4), shared by every
    /// sink targeting this shard (the supervisor's assignment sink and
    /// each reader/writer sidecar); cleared at the top of the shard's
    /// `on_inbound_ready`.
    pub wake_dirty: Arc<AtomicBool>,
    pub connections: HashMap<ConnId, ConnState>,
    pub next_conn_id: ConnId,
    /// Dispatch-correlation → open response socket. Populated on
    /// dispatch; cleared on reply, settlement, timeout, or close.
    pub in_flight: HashMap<u64, PendingRequest>,
    /// Credit-window depth (ADR-0128): the count of in-flight response
    /// chunks a stream may hold; also the bounded hand-off channel's
    /// capacity and the initial credit grant.
    pub response_stream_window: u32,
    /// Active response streams (ADR-0128), keyed by `stream_id` (== the
    /// request's dispatch correlation id). Promoted from `in_flight` on
    /// `HttpResponseStreamOpen`; torn down on stream end, flood, timeout, or
    /// connection close.
    pub streams: HashMap<u64, StreamState>,
    /// Inbound request-stream credit-window depth (ADR-0128): the initial
    /// count of `HttpRequestChunk` mails the cap delivers to a streaming
    /// handler before parking the reader on the handler's `HttpRequestCredit`.
    pub request_stream_window: u32,
    /// Active *inbound* request streams (ADR-0128), keyed by a cap-minted
    /// `stream_id`. Created when a streaming handler accepts a request head;
    /// torn down when the body ends (the final response then rides the
    /// `HttpRequestStreamEnd` dispatch through `in_flight`) or on connection
    /// close.
    pub request_streams: HashMap<u64, RequestStreamState>,
    /// Monotonic source of cap-minted stream ids (inbound request streams
    /// and websocket outbound streams), shared across every shard
    /// (ADR-0135): a handler identifies a connection by its `stream_id`
    /// alone (ADR-0132), so ids must stay unique across the whole cap, not
    /// per shard. Distinct from response stream ids (those reuse the
    /// request's dispatch correlation, unique substrate-wide already), so
    /// the tables never collide.
    pub next_stream_id: Arc<AtomicU64>,
    /// Read deadline between frames on an upgraded websocket connection
    /// (ADR-0129), from `websocket_idle_timeout_millis` — longer than
    /// `request_timeout`, since an idle websocket is normal.
    pub ws_idle_timeout: Duration,
}

/// Write a canned refusal to a just-accepted socket and shut it down —
/// the pre-adoption reject (no `ConnState` exists yet), used by the
/// supervisor's ceiling check and its no-shard fallback.
fn refuse_connection(mut stream: TcpStream, status: u16, message: &str) {
    let bytes = render_status_response(status, message);
    let _ = stream.write_all(&bytes).and_then(|()| stream.flush());
    let _ = stream.shutdown(Shutdown::Both);
}

impl HttpSupervisorState {
    /// Spawn the dispatch shards (ADR-0135) — deferred to the first accepted
    /// connection because `init` has no dispatcher ctx to spawn children
    /// from. `dispatch_shards == 0` sizes automatically, mirroring the
    /// scheduler pool's worker default (`available_parallelism - 1`).
    pub fn ensure_shards(&mut self, ctx: &mut NativeCtx<'_>) {
        if !self.shards.is_empty() {
            return;
        }
        let count = if self.config.dispatch_shards == 0 {
            thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1))
        } else {
            self.config.dispatch_shards
        };
        for index in 0..count {
            let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
            let wake_dirty = Arc::new(AtomicBool::new(false));
            let seed = HttpShardSeed {
                inbound_rx: Some(inbound_rx),
                inbound_tx: inbound_tx.clone(),
                wake_dirty: Arc::clone(&wake_dirty),
                routes: Arc::clone(&self.routes),
                live_connections: Arc::clone(&self.live_connections),
                handler_mailbox: self.config.handler_mailbox.clone(),
                max_request_bytes: self.config.max_request_bytes,
                max_header_bytes: self.config.max_header_bytes,
                request_timeout: Duration::from_millis(self.config.request_timeout_millis),
                keep_alive_timeout: Duration::from_millis(self.config.keep_alive_timeout_millis),
                ws_idle_timeout: Duration::from_millis(self.config.websocket_idle_timeout_millis),
                response_stream_window: self.config.response_stream_window,
                request_stream_window: self.config.request_stream_window,
                next_stream_id: Arc::clone(&self.next_stream_id),
            };
            let subname = format!("shard-{index}");
            match ctx
                .spawn_child::<HttpDispatchShard>(Subname::Named(&subname), seed)
                .finish()
            {
                Ok(mailbox) => self.shards.push(WakeSink {
                    inbound_tx,
                    mailer: Arc::clone(&self.mailer),
                    self_id: mailbox,
                    wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
                    dirty: wake_dirty,
                }),
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::http_server",
                        shard = %subname,
                        error = ?e,
                        "http dispatch shard spawn failed",
                    );
                }
            }
        }
        tracing::info!(
            target: "aether_substrate::http_server",
            port = self.listener_port,
            shards = self.shards.len(),
            "http dispatch shards spawned",
        );
    }

    /// Adopt one accepted connection: enforce the global ceiling, pick the
    /// next shard round-robin, and hand the socket over. Refuses `503`
    /// before any reader thread exists (ADR-0108 §6) when the table is at
    /// capacity or no shard came up.
    pub fn assign_peer(&mut self, ctx: &mut NativeCtx<'_>, stream: TcpStream, peer: SocketAddr) {
        self.ensure_shards(ctx);
        if self.shards.is_empty() {
            refuse_connection(stream, 503, "no dispatch shards");
            tracing::warn!(
                target: "aether_substrate::http_server",
                %peer,
                "http conn refused: no dispatch shards",
            );
            return;
        }
        if self.live_connections.load(Ordering::Acquire) >= self.config.max_connections {
            refuse_connection(stream, 503, "server at connection capacity");
            tracing::warn!(
                target: "aether_substrate::http_server",
                %peer,
                live = self.live_connections.load(Ordering::Acquire),
                "http conn refused: at capacity",
            );
            return;
        }
        self.live_connections.fetch_add(1, Ordering::AcqRel);
        let index = self.next_shard % self.shards.len();
        self.next_shard = self.next_shard.wrapping_add(1);
        if !self.shards[index].post(InboundEvent::PeerAccepted { stream, peer }) {
            // The shard's receiver is gone — teardown is in progress; the
            // socket just dropped with the event.
            self.live_connections.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Claim `(prefix, method)` for `mailbox`, dispatching as `kind`
    /// (ADR-0130). A key held by a different mailbox is answered
    /// `Err`; the same mailbox re-claiming its own key is an
    /// idempotent `Ok` that updates `kind` — so a component
    /// re-running `wire` after `replace_component` re-registers
    /// cleanly (its `MailboxId` is stable).
    ///
    /// # Panics
    /// Panics if the route-table `RwLock` is poisoned — fail-fast per
    /// ADR-0063 (a poisoned table means a supervisor or shard already
    /// panicked mid-read/write).
    pub fn register_route(
        &mut self,
        prefix: &str,
        method: Option<HttpMethod>,
        kind: KindId,
        mailbox: MailboxId,
    ) -> RegisterRouteResult {
        let prefix = match normalize_prefix(prefix) {
            Ok(prefix) => prefix,
            Err(error) => return RegisterRouteResult::Err { error },
        };
        let mut routes = self.routes.write().expect("route table lock poisoned");
        if let Some(existing) = routes
            .iter_mut()
            .find(|r| r.prefix == prefix && r.method == method)
        {
            if existing.mailbox == mailbox {
                existing.kind = kind;
                return RegisterRouteResult::Ok;
            }
            return RegisterRouteResult::Err {
                error: format!(
                    "route ({prefix:?}, {method:?}) already claimed by mailbox {:?}",
                    existing.mailbox,
                ),
            };
        }
        routes.push(Route {
            prefix,
            method,
            kind,
            mailbox,
        });
        RegisterRouteResult::Ok
    }

    /// Release the `(prefix, method)` route held by `mailbox`.
    /// Idempotent — releasing a route that isn't held (or is held by
    /// someone else) is still `Ok`, mirroring the input cap's
    /// unsubscribe semantics.
    ///
    /// # Panics
    /// Panics if the route-table `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub fn unregister_route(
        &mut self,
        prefix: &str,
        method: Option<HttpMethod>,
        mailbox: MailboxId,
    ) -> RegisterRouteResult {
        let prefix = match normalize_prefix(prefix) {
            Ok(prefix) => prefix,
            Err(error) => return RegisterRouteResult::Err { error },
        };
        self.routes
            .write()
            .expect("route table lock poisoned")
            .retain(|r| !(r.prefix == prefix && r.method == method && r.mailbox == mailbox));
        RegisterRouteResult::Ok
    }

    /// Release every route held by `mailbox` (ADR-0130's
    /// `UnregisterRoutesAll`).
    ///
    /// # Panics
    /// Panics if the route-table `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub fn unregister_routes_all(&mut self, mailbox: MailboxId) {
        self.routes
            .write()
            .expect("route table lock poisoned")
            .retain(|r| r.mailbox != mailbox);
    }
}

impl HttpShardState {
    /// The longest segment-boundary prefix match among
    /// method-compatible routes, with a method-specific route beating
    /// a method-agnostic one at equal prefix (ADR-0130), copied out
    /// from under the shared table's read lock. `None` ⇒ the
    /// configured `handler_mailbox` fallback.
    ///
    /// # Panics
    /// Panics if the route-table `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    fn resolve_route(&self, path: &str, method: HttpMethod) -> Option<(MailboxId, KindId)> {
        self.routes
            .read()
            .expect("route table lock poisoned")
            .iter()
            .filter(|r| r.method.is_none_or(|m| m == method) && route_matches(&r.prefix, path))
            .max_by_key(|r| (r.prefix.len(), r.method.is_some()))
            .map(|r| (r.mailbox, r.kind))
    }

    /// Release this connection's slot in the global live count (ADR-0135).
    /// Paired with the supervisor's assignment-time increment; called
    /// exactly once per assigned connection — on close, or on an adoption
    /// failure before any `ConnState` exists.
    fn release_connection_slot(&self) {
        self.live_connections.fetch_sub(1, Ordering::AcqRel);
    }

    /// Allocate a fresh `ConnId`, store the connection's write half, and
    /// spin a reader thread for the read half. The global `max_connections`
    /// ceiling was already enforced at assignment by the supervisor
    /// (ADR-0135); an adoption failure here releases the slot it charged.
    pub fn spawn_reader_for_peer(&mut self, stream: TcpStream, peer: SocketAddr) {
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
                self.release_connection_slot();
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
            self.release_connection_slot();
            return;
        }
        let write_half = stream;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);
        // Per-connection reader control channel (ADR-0128 + keep-alive): the
        // dispatcher's half is stored in `ConnState`, the reader's half moves
        // into the thread.
        let (control_tx, control_rx) = mpsc::channel::<ReaderControl>();

        let sink = WakeSink {
            inbound_tx: self.inbound_tx.clone(),
            mailer: Arc::clone(&self.mailer),
            self_id: self.self_mailbox,
            wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
            dirty: Arc::clone(&self.wake_dirty),
        };
        let tuning = ReaderTuning {
            request_timeout: self.request_timeout,
            idle_timeout: self.keep_alive_timeout,
            max_header_bytes: self.max_header_bytes,
            ws_idle_timeout: self.ws_idle_timeout,
            ws_max_message_bytes: self.max_request_bytes,
        };

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
                    &control_rx,
                    tuning,
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
                self.release_connection_slot();
                return;
            }
        };

        self.connections.insert(
            conn_id,
            ConnState {
                peer,
                write_half,
                shutdown,
                control_tx,
                active_stream: None,
                reader_thread: Some(thread),
                ws_pending_key: None,
                websocket: None,
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
    pub fn dispatch_request(
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
        let keep_alive = request.keep_alive;
        // Route resolution (ADR-0130): a registered route names its
        // handler by stable `MailboxId` and the kind its requests
        // dispatch as; a dead routed mailbox is answered `503`, the
        // same surface as an unresolved fallback (falling back instead
        // would silently reroute a claimed path family). No route ⇒
        // the ADR-0108 §3 late-binding fallback: resolve the configured
        // `handler_mailbox` by name at dispatch time through the
        // registry — the sanctioned runtime-name path — dispatching
        // the generic request kind. Nothing live either way → `503`.
        let routed = self.resolve_route(&request.path, method);
        let (handler, dispatch_kind) = if let Some((mailbox, kind)) = routed {
            if validate_route_mailbox(self.mailer.registry(), mailbox).is_err() {
                self.write_status_response(conn_id, 503, "routed handler gone");
                self.close_connection(conn_id, "routed mailbox dead");
                return;
            }
            (mailbox, kind)
        } else if let Some(handler) = self.mailer.registry().lookup(&self.handler_mailbox) {
            (handler, <HttpServerRequest as Kind>::ID)
        } else {
            self.write_status_response(conn_id, 503, "no handler registered");
            self.close_connection(conn_id, "handler unresolved");
            return;
        };
        let peer_addr = self
            .connections
            .get(&conn_id)
            .map(|c| c.peer.to_string())
            .unwrap_or_default();
        let payload = HttpServerRequest {
            method,
            path: request.path,
            query: request.query,
            headers: request.headers,
            body: request.body,
            peer_addr,
        }
        .encode_into_bytes();
        let mail_id = ctx.send_envelope_detached(handler, dispatch_kind, &payload);
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
        self.in_flight.insert(
            mail_id.correlation_id,
            PendingRequest {
                conn_id,
                method,
                keep_alive,
                handler,
            },
        );
    }

    /// Resolve the handler a request head dispatches to — a matching ADR-0130
    /// route (its stable mailbox validated live, no fallback if it is dead) or
    /// the late-bound `handler_mailbox` — read-only, so the streaming decision
    /// can consult the resolved handler's accept-set before the body is read.
    /// `None` collapses every "nothing live" case to the `503` the buffered
    /// dispatch answers.
    fn resolved_handler(&self, path: &str, method: HttpMethod) -> Option<(MailboxId, KindId)> {
        if let Some((mailbox, kind)) = self.resolve_route(path, method) {
            if validate_route_mailbox(self.mailer.registry(), mailbox).is_err() {
                return None;
            }
            return Some((mailbox, kind));
        }
        self.mailer
            .registry()
            .lookup(&self.handler_mailbox)
            .map(|handler| (handler, <HttpServerRequest as Kind>::ID))
    }

    /// Decide a parsed request head's body path (ADR-0128): resolve the
    /// handler, read its accept-set, and either signal the parked reader to
    /// buffer or stream the body, or reject and close. A handler whose
    /// accept-set carries `HttpRequestStreamOpen` takes the streamed path
    /// structurally — the cap cannot stream chunk kinds to a handler that does
    /// not handle them — so no per-request opt-in is needed.
    pub fn decide_request_head(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        head: ParsedHead,
    ) {
        let Some(method) = parse_http_method(&head.method) else {
            self.write_status_response(conn_id, 501, "method not implemented");
            self.close_connection(conn_id, "unsupported method");
            return;
        };
        let Some((handler, _kind)) = self.resolved_handler(&head.path, method) else {
            self.write_status_response(conn_id, 503, "no handler registered");
            self.close_connection(conn_id, "handler unresolved");
            return;
        };
        // ADR-0129: a websocket upgrade handshake. The cap validates the
        // protocol layer (`426`/`400`, cap-owned) before dispatch; on a valid
        // handshake it stashes the `Sec-WebSocket-Key` and dispatches the
        // request as an ordinary buffered `HttpServerRequest` the handler
        // answers with `WebSocketAccept` (accept) or `HttpServerResponse`
        // (decline). An upgrade request carries no body, so it always buffers.
        if header_has_token(&head.headers, "upgrade", "websocket") {
            match validate_ws_handshake(&head.headers) {
                Ok(key) => {
                    if let Some(conn) = self.connections.get_mut(&conn_id) {
                        conn.ws_pending_key = Some(key);
                    }
                    self.signal_reader(conn_id, ReaderControl::Buffered);
                }
                Err((status, message)) => {
                    self.write_status_response(conn_id, status, message);
                    self.close_connection(conn_id, "invalid websocket handshake");
                }
            }
            return;
        }
        let streaming = self
            .mailer
            .capability_registry()
            .accepts(handler, <HttpRequestStreamOpen as Kind>::ID);
        match (head.framing, streaming) {
            // Smuggling (`Content-Length` + `Transfer-Encoding`) and a
            // non-`chunked` coding are always `411`; a lone `chunked` body to a
            // buffered handler has no length to buffer under, also `411`
            // (relaxing #2545's guard only for a streaming handler).
            (BodyFraming::Invalid, _) | (BodyFraming::Chunked, false) => {
                self.write_status_response(conn_id, 411, "length required");
                self.close_connection(conn_id, "unbufferable request framing");
            }
            (BodyFraming::Length(n), false) if n > self.max_request_bytes => {
                self.write_status_response(conn_id, 413, "request body exceeds limit");
                self.close_connection(conn_id, "body exceeds limit");
            }
            (_, false) => {
                // Buffered handler: the reader reads the `Content-Length` body
                // into one `RequestParsed`, then `dispatch_request` runs.
                self.signal_reader(conn_id, ReaderControl::Buffered);
            }
            (_, true) => {
                self.start_request_stream(ctx, conn_id, handler, method, head);
            }
        }
    }

    /// Send a control message to a connection's parked reader; a send failure
    /// means the reader already exited, so the connection is closed.
    pub fn signal_reader(&mut self, conn_id: ConnId, control: ReaderControl) {
        let sent = self
            .connections
            .get(&conn_id)
            .is_some_and(|conn| conn.control_tx.send(control).is_ok());
        if !sent {
            self.close_connection(conn_id, "reader gone");
        }
    }

    /// Format + write the handler's [`HttpServerResponse`]. `is_head`
    /// suppresses the response body per HEAD semantics (headers,
    /// including `Content-Length`, still describe what the body would
    /// have been).
    pub fn write_handler_response(
        &mut self,
        conn_id: ConnId,
        response: &HttpServerResponse,
        is_head: bool,
        keep_alive: bool,
    ) {
        let bytes = render_handler_response(response, is_head, keep_alive);
        self.write_raw_to(conn_id, &bytes);
    }

    /// Release the reader for the next request on a kept-alive connection by
    /// signalling its resume channel. A send failure means the reader
    /// already exited (its own read error / EOF), so the connection is
    /// closed instead.
    pub fn resume_connection(&mut self, conn_id: ConnId) {
        self.signal_reader(conn_id, ReaderControl::Resume);
    }

    /// Format + write a canned status response (the cap's own
    /// `413` / `431` / `501` / `502` / `503` / `504`).
    pub fn write_status_response(&mut self, conn_id: ConnId, status: u16, message: &str) {
        let bytes = render_status_response(status, message);
        self.write_raw_to(conn_id, &bytes);
    }

    pub fn write_raw_to(&mut self, conn_id: ConnId, bytes: &[u8]) {
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

    pub fn close_connection(&mut self, conn_id: ConnId, reason: &str) {
        let Some(mut conn) = self.connections.remove(&conn_id) else {
            return;
        };
        self.release_connection_slot();
        conn.shutdown.store(true, Ordering::Release);
        let _ = conn.write_half.shutdown(Shutdown::Both);
        // Detach the reader without joining inline — the dispatcher must
        // not block on it. The thread sees the shutdown (or its own EOF)
        // and exits; the JoinHandle drop detaches.
        drop(conn.reader_thread.take());
        // Tear down any response stream bound to this connection (ADR-0128).
        // The socket shutdown above unblocks a write-blocked writer; dropping
        // the sender (in `teardown_stream`) unblocks a recv-blocked one.
        let stream_ids: Vec<u64> = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.conn_id == conn_id)
            .map(|(id, _)| *id)
            .collect();
        for stream_id in stream_ids {
            self.teardown_stream(stream_id);
        }
        // Drop any inbound request stream bound to this connection (ADR-0128);
        // the reader (parked on the control channel or blocked mid-read) is
        // already unblocked by the dropped `ConnState` sender / socket
        // shutdown above.
        self.request_streams
            .retain(|_, stream| stream.conn_id != conn_id);
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
