// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

/// `aether.http.server` runtime state. Owns one TCP listener + per-connection
/// state + the in-flight correlation table. The dispatcher holds this as the
/// cap's state and routes envelopes through the macro-emitted `Dispatch`
/// impl; the addressing identity is the distinct ZST `HttpServerCapability`.
/// Living in this private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct HttpServerCapabilityState {
    pub handler_mailbox: String,
    /// Registered routes (ADR-0130), unordered — [`Self::resolve_route`]
    /// picks the winner per request by `(prefix length, method
    /// specificity)`, which is deterministic without a sort: two
    /// distinct equal-length prefixes cannot both match one path, and
    /// duplicate `(prefix, method)` keys are rejected at registration.
    /// Route counts are tens per substrate, so the linear scan is
    /// dwarfed by the header parse that precedes it (ADR-0130).
    pub routes: Vec<Route>,
    pub max_request_bytes: usize,
    pub max_header_bytes: usize,
    /// Live-connection-table ceiling (ADR-0108 §6); a peer accepted past
    /// this is refused `503` before a reader thread is spawned.
    pub max_connections: usize,
    pub request_timeout: Duration,
    /// Idle timeout between requests on a kept-alive connection (and for a
    /// fresh connection that never sends its first byte). Distinct from
    /// `request_timeout`, which stays the in-flight read + response
    /// deadline.
    pub keep_alive_timeout: Duration,
    pub self_mailbox: MailboxId,
    /// Cached `Arc<Mailer>` so the dispatcher can fire wake mails into
    /// the cap, resolve the handler mailbox by name at dispatch time,
    /// and subscribe to settlement. The cap is single-threaded
    /// post-ADR-0038 so direct storage is fine.
    pub mailer: Arc<Mailer>,
    pub listener_port: u16,
    pub accept_shutdown: Arc<AtomicBool>,
    pub accept_thread: Option<JoinHandle<()>>,
    pub inbound_rx: mpsc::Receiver<InboundEvent>,
    pub inbound_tx: mpsc::Sender<InboundEvent>,
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
    /// Monotonic source of inbound request-stream ids. Distinct from response
    /// stream ids (those reuse the request's dispatch correlation), so the two
    /// tables never collide.
    pub next_stream_id: u64,
    /// Read deadline between frames on an upgraded websocket connection
    /// (ADR-0129), from `websocket_idle_timeout_millis` — longer than
    /// `request_timeout`, since an idle websocket is normal.
    pub ws_idle_timeout: Duration,
}

impl HttpServerCapabilityState {
    /// Claim `(prefix, method)` for `mailbox`, dispatching as `kind`
    /// (ADR-0130). A key held by a different mailbox is answered
    /// `Err`; the same mailbox re-claiming its own key is an
    /// idempotent `Ok` that updates `kind` — so a component
    /// re-running `wire` after `replace_component` re-registers
    /// cleanly (its `MailboxId` is stable).
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
        if let Some(existing) = self
            .routes
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
        self.routes.push(Route {
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
            .retain(|r| !(r.prefix == prefix && r.method == method && r.mailbox == mailbox));
        RegisterRouteResult::Ok
    }

    /// The longest segment-boundary prefix match among
    /// method-compatible routes, with a method-specific route beating
    /// a method-agnostic one at equal prefix (ADR-0130). `None` ⇒ the
    /// configured `handler_mailbox` fallback.
    fn resolve_route(&self, path: &str, method: HttpMethod) -> Option<&Route> {
        self.routes
            .iter()
            .filter(|r| r.method.is_none_or(|m| m == method) && route_matches(&r.prefix, path))
            .max_by_key(|r| (r.prefix.len(), r.method.is_some()))
    }

    /// Allocate a fresh `ConnId`, store the connection's write half, and
    /// spin a reader thread for the read half. Refuses `503` and closes
    /// without spawning a reader when the live connection table is
    /// already at `max_connections` (ADR-0108 §6) — `connections` is the
    /// single authoritative live-connection count, so no separate
    /// counter is kept.
    pub fn spawn_reader_for_peer(&mut self, mut stream: TcpStream, peer: SocketAddr) {
        if self.connections.len() >= self.max_connections {
            let bytes = render_status_response(503, "server at connection capacity");
            let _ = stream.write_all(&bytes).and_then(|()| stream.flush());
            let _ = stream.shutdown(Shutdown::Both);
            tracing::warn!(
                target: "aether_substrate::http_server",
                %peer,
                live = self.connections.len(),
                "http conn refused: at capacity",
            );
            return;
        }

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
        // Per-connection reader control channel (ADR-0128 + keep-alive): the
        // dispatcher's half is stored in `ConnState`, the reader's half moves
        // into the thread.
        let (control_tx, control_rx) = mpsc::channel::<ReaderControl>();

        let sink = WakeSink {
            inbound_tx: self.inbound_tx.clone(),
            mailer: Arc::clone(&self.mailer),
            self_id: self.self_mailbox,
            wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
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
        let routed = self
            .resolve_route(&request.path, method)
            .map(|route| (route.mailbox, route.kind));
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
        if let Some(route) = self.resolve_route(path, method) {
            if validate_route_mailbox(self.mailer.registry(), route.mailbox).is_err() {
                return None;
            }
            return Some((route.mailbox, route.kind));
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
