// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

use std::time::Instant;

/// Outcome of [`read_more`].
pub enum ReadStep {
    Filled(usize),
    Eof,
    Timeout,
    Error(String),
}

/// One bounded read off the socket, retrying past `Interrupted` and
/// folding `WouldBlock` / `TimedOut` (the `set_read_timeout` expiry)
/// into [`ReadStep::Timeout`].
pub fn read_more(stream: &mut TcpStream, chunk: &mut [u8]) -> ReadStep {
    loop {
        match stream.read(chunk) {
            Ok(0) => return ReadStep::Eof,
            Ok(n) => return ReadStep::Filled(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
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
    /// How the body is framed (ADR-0128). The dispatcher turns this into the
    /// buffered / streamed / `411` decision; the reader turns it into the body
    /// read (count down `content_length`, or decode chunked).
    framing: BodyFraming,
    /// httparse minor version: `Some(0)` = HTTP/1.0, `Some(1)` = HTTP/1.1.
    /// Drives the keep-alive default ([`request_keeps_alive`]).
    version: Option<u8>,
}

/// Outcome of [`parse_head`].
enum HeadParse {
    Complete(RequestHead),
    NeedMore,
    Reject { status: u16, message: &'static str },
}

/// Percent-decode an RFC 3986 path component (ADR-0108 §2: "the decoded
/// path component"). A `%XX` escape with two following hex digits decodes
/// to the byte; a short or invalid escape (a trailing `%`, `%2`, or
/// non-hex digits like `%zz`) passes through literally rather than
/// erroring. The query string is not run through this — `+`-as-space is
/// form-encoding semantics that belongs to the query, not the path.
pub fn percent_decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                // `hi` / `lo` are each a single hex digit (0..=15), so
                // `hi * 16 + lo` is always in 0..=255.
                out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse the accumulated bytes as an HTTP/1.1 request head (request line
/// and headers). Enforces the header-count cap (`431` via
/// `TooManyHeaders`) and surfaces the header-byte cap to the caller via
/// [`HeadParse::NeedMore`] (the caller rejects `431` once `buf` outgrows
/// `max_header_bytes`).
fn parse_head(buf: &[u8], max_header_bytes: usize) -> HeadParse {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADER_COUNT];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(buf) {
        Ok(httparse::Status::Complete(head_len)) => {
            let method = request.method.unwrap_or_default().to_string();
            let version = request.version;
            let raw_path = request.path.unwrap_or("/");
            let (path, query) = match raw_path.split_once('?') {
                Some((before, after)) => (percent_decode_path(before), after.to_string()),
                None => (percent_decode_path(raw_path), String::new()),
            };
            let mut out_headers = Vec::with_capacity(request.headers.len());
            let mut content_length = 0usize;
            let mut has_content_length = false;
            let mut bad_length = false;
            let mut has_transfer_encoding = false;
            let mut transfer_encoding_chunked = false;
            for header in &*request.headers {
                let value = String::from_utf8_lossy(header.value).into_owned();
                if header.name.eq_ignore_ascii_case("content-length") {
                    has_content_length = true;
                    match value.trim().parse::<usize>() {
                        Ok(n) => content_length = n,
                        Err(_) => bad_length = true,
                    }
                }
                if header.name.eq_ignore_ascii_case("transfer-encoding") {
                    has_transfer_encoding = true;
                    // A lone `chunked` coding is streamable; any list (e.g.
                    // `gzip, chunked`) or non-`chunked` coding is refused.
                    if value.trim().eq_ignore_ascii_case("chunked") {
                        transfer_encoding_chunked = true;
                    }
                }
                out_headers.push(HttpHeader { name: header.name.to_string(), value });
            }
            if bad_length {
                return HeadParse::Reject { status: 400, message: "invalid content-length" };
            }
            // Body framing (ADR-0128): `Content-Length` + `Transfer-Encoding`
            // together is request smuggling and a non-`chunked` coding is
            // unsupported — both `Invalid` (the dispatcher answers `411`); a
            // lone `chunked` streams; otherwise the `Content-Length` count (0
            // when absent) delimits the body.
            let framing = if has_transfer_encoding {
                if has_content_length || !transfer_encoding_chunked {
                    BodyFraming::Invalid
                } else {
                    BodyFraming::Chunked
                }
            } else {
                BodyFraming::Length(content_length)
            };
            HeadParse::Complete(RequestHead {
                head_len,
                method,
                path,
                query,
                headers: out_headers,
                content_length,
                framing,
                version,
            })
        }
        Ok(httparse::Status::Partial) => {
            if buf.len() > max_header_bytes {
                HeadParse::Reject { status: 431, message: "request header fields too large" }
            } else {
                HeadParse::NeedMore
            }
        }
        Err(httparse::Error::TooManyHeaders) => HeadParse::Reject { status: 431, message: "too many request headers" },
        Err(_) => HeadParse::Reject { status: 400, message: "malformed request" },
    }
}

/// Per-connection reader tuning, grouped so the reader thread body takes one
/// bundle rather than four scalars: the in-flight read + response deadline,
/// the idle timeout between requests, and the request byte caps.
#[derive(Copy, Clone)]
pub struct ReaderTuning {
    pub request_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_header_bytes: usize,
    /// Cap on a buffered request body (ADR-0108 §6); the reader answers
    /// `413` itself past it (ADR-0135 §2).
    pub max_request_bytes: usize,
    /// Read deadline between frames once the connection upgrades to a websocket
    /// (ADR-0129). The frame loop reads under this rather than `request_timeout`.
    pub ws_idle_timeout: Duration,
    /// Cap on a single reassembled websocket message (ADR-0129), reusing the
    /// request-body cap — an inbound message past this is a protocol error the
    /// reader answers with a close rather than buffering unboundedly.
    pub ws_max_message_bytes: usize,
}

/// Whether the `Connection` header names `token` (case-insensitive), across
/// comma-separated values and repeated header lines (`Connection: keep-alive,
/// Upgrade`).
fn connection_has_token(headers: &[HttpHeader], token: &str) -> bool {
    header_has_token(headers, "connection", token)
}

/// First value of the header named `name` (case-insensitive), or `None` if the
/// request carries no such header. Trimmed of surrounding whitespace.
fn first_header_value<'a>(headers: &'a [HttpHeader], name: &str) -> Option<&'a str> {
    headers.iter().find(|header| header.name.eq_ignore_ascii_case(name)).map(|header| header.value.trim())
}

/// Whether the header named `name` lists `token` (case-insensitive), across
/// comma-separated values and repeated header lines — the general form of
/// [`connection_has_token`], used for `Upgrade: websocket`.
pub fn header_has_token(headers: &[HttpHeader], name: &str, token: &str) -> bool {
    headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name))
        .any(|header| header.value.split(',').any(|value| value.trim().eq_ignore_ascii_case(token)))
}

/// Validate an RFC 6455 upgrade handshake's protocol layer (ADR-0129 §1),
/// cap-owned before any dispatch. The caller has already seen `Upgrade:
/// websocket`; this checks `Connection: Upgrade`, `Sec-WebSocket-Version: 13`,
/// and a non-empty `Sec-WebSocket-Key`, returning the key on success or the
/// canned status the cap answers (`426` for a wrong version, `400` for a
/// malformed handshake) on failure.
pub fn validate_ws_handshake(headers: &[HttpHeader]) -> Result<String, (u16, &'static str)> {
    if !connection_has_token(headers, "upgrade") {
        return Err((400, "websocket upgrade requires Connection: Upgrade"));
    }
    if first_header_value(headers, "sec-websocket-version") != Some("13") {
        return Err((426, "unsupported websocket version"));
    }
    match first_header_value(headers, "sec-websocket-key") {
        Some(key) if !key.is_empty() => Ok(key.to_string()),
        _ => Err((400, "missing sec-websocket-key")),
    }
}

/// Whether a request wants its connection kept alive after the response.
/// An explicit `Connection` token wins either way; absent one, the HTTP
/// version decides — HTTP/1.1 (`Some(1)`) keeps alive by default, HTTP/1.0
/// (`Some(0)`, or an unknown/absent version) closes by default.
pub fn request_keeps_alive(version: Option<u8>, headers: &[HttpHeader]) -> bool {
    if connection_has_token(headers, "close") {
        return false;
    }
    if connection_has_token(headers, "keep-alive") {
        return true;
    }
    matches!(version, Some(1))
}

/// Cap on a chunked-transfer size line (hex size + optional `;ext`) and on a
/// trailer header line, in bytes — a peer cannot stream an unbounded framing
/// line at the cap.
const MAX_CHUNK_LINE_BYTES: usize = 256;
/// Cap on trailer header lines after the terminating chunk — a peer cannot
/// stream unbounded trailers.
const MAX_CHUNK_TRAILERS: usize = MAX_HEADER_COUNT;

/// Reader-side handler resolution (ADR-0135 §2): the winning route's
/// next live member by this reader's round-robin cursor (seeded from
/// the connection id so concurrent connections start spread across a
/// shared set, ADR-0136), or the late-bound `handler_mailbox`
/// fallback. `streaming` is the resolved handler's accept-set verdict
/// on [`HttpRequestStreamOpen`] — the same structural opt-in the
/// dispatcher used to read.
enum ReaderResolution {
    /// A live handler: the dispatch target, its dispatch kind, and
    /// whether it takes the streamed body path.
    Live { handler: MailboxId, kind: KindId, streaming: bool },
    /// A route matched but no member of its set is live — `503`, never
    /// the fallback (that would silently reroute a claimed family).
    Dead,
    /// No route and no resolvable fallback — `503`.
    NoHandler,
}

fn resolve_at_reader(
    shared: &ReaderShared,
    sink: &WakeSink,
    cursor: &mut usize,
    path: &str,
    method: HttpMethod,
) -> ReaderResolution {
    let registry = sink.mailer.registry();
    let picked = {
        let routes = shared.routes.read().expect("route table lock poisoned");
        match best_route(&routes, path, method) {
            Some(route) => {
                let start = *cursor;
                *cursor = cursor.wrapping_add(1);
                let len = route.members.len();
                let mut live = None;
                for offset in 0..len {
                    let member = route.members[(start + offset) % len];
                    if validate_route_mailbox(registry, member).is_ok() {
                        live = Some((member, route.kind));
                        break;
                    }
                }
                let Some(found) = live else {
                    return ReaderResolution::Dead;
                };
                Some(found)
            }
            None => shared
                .handler_mailbox
                .filter(|&id| validate_route_mailbox(registry, id).is_ok())
                .map(|id| (id, <HttpServerRequest as Kind>::ID)),
        }
    };
    match picked {
        Some((handler, kind)) => ReaderResolution::Live {
            handler,
            kind,
            streaming: sink.mailer.capability_registry().accepts(handler, <HttpRequestStreamOpen as Kind>::ID),
        },
        None => ReaderResolution::NoHandler,
    }
}

/// Re-arm the socket read timeout to `want` if it differs from `current`,
/// posting `ReaderClosed` and returning `false` on a `set_read_timeout`
/// failure (the caller must bail); returns `true` otherwise, with
/// `current` updated to `want` when a change was made.
fn ensure_read_timeout(
    stream: &mut TcpStream,
    sink: &WakeSink,
    conn_id: ConnId,
    current: &mut Duration,
    want: Duration,
) -> bool {
    if want == *current {
        return true;
    }
    if stream.set_read_timeout(Some(want)).is_err() {
        sink.post(InboundEvent::ReaderClosed { conn_id, reason: "set read timeout failed".to_string() });
        return false;
    }
    *current = want;
    true
}

/// Reader-written reject (ADR-0135 §2): write the canned status on the
/// reader's own full-duplex clone — no response can be in flight at a
/// pre-dispatch reject — and post `ReaderClosed` so the shard reaps the
/// connection state.
fn reject_and_close(stream: &mut TcpStream, sink: &WakeSink, conn_id: ConnId, status: u16, message: &'static str) {
    let bytes = render_status_response(status, message);
    let _ = stream.write_all(&bytes).and_then(|()| stream.flush());
    sink.post(InboundEvent::ReaderClosed { conn_id, reason: message.to_string() });
}

/// Per-connection reader thread body. An outer per-request loop reads one
/// HTTP/1.1 request head and makes the request-path decision itself
/// (ADR-0135 §2): it resolves the handler against the shared route
/// table + registry, writes every pre-dispatch reject on its own
/// socket clone, and — the common buffered case — reads the body,
/// encodes the request payload, and posts one ready-to-dispatch
/// [`InboundEvent::RequestParsed`]. Only a streaming-handler body
/// takes the head round trip ([`InboundEvent::RequestHeadParsed`] →
/// [`ReaderControl::Stream`], ADR-0128); a websocket handshake is
/// validated here and rides the buffered path with its key attached.
/// After dispatch the reader parks on the control channel: on a
/// keep-alive response the dispatcher signals [`ReaderControl::Resume`]
/// and the loop reads the next request off the same socket (carrying
/// any over-read pipelined bytes forward); on a close response the
/// dispatcher drops the sender and the reader exits. A fresh / idle
/// connection between requests reads under `idle_timeout`; once a
/// request's bytes start arriving the in-flight `request_timeout`
/// (slow-loris) governs, and the handler-response deadline is the
/// control wait.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn run_reader_loop(
    read_half: TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    sink: &WakeSink,
    control_rx: &mpsc::Receiver<ReaderControl>,
    tuning: ReaderTuning,
    shared: &ReaderShared,
) {
    let ReaderTuning { request_timeout, idle_timeout, max_header_bytes, max_request_bytes, .. } = tuning;
    let mut stream = read_half;
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    // Round-robin cursor over shared route member sets (ADR-0136),
    // seeded from the connection id so concurrent connections start
    // spread across a set rather than all on member 0.
    let mut route_cursor = usize::try_from(conn_id).unwrap_or(0);
    // `spawn_reader_for_peer` set the socket read timeout to `request_timeout`
    // before spawning; track it so a read only re-issues `set_read_timeout`
    // when the desired timeout actually changes.
    let mut current_timeout = request_timeout;

    // Outer per-request loop (keep-alive): serve requests in sequence on one
    // socket, one in flight at a time.
    loop {
        // Phase 1: accumulate the request head. Before the first byte of a
        // new request the read waits under the idle timeout (a kept-alive /
        // fresh connection between requests); once head bytes are buffered
        // it waits under the in-flight request timeout (slow-loris).
        let head = loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match parse_head(&buf, max_header_bytes) {
                HeadParse::Complete(head) => break head,
                HeadParse::Reject { status, message } => {
                    reject_and_close(&mut stream, sink, conn_id, status, message);
                    return;
                }
                HeadParse::NeedMore => {}
            }
            let want_timeout = if buf.is_empty() {
                idle_timeout
            } else {
                request_timeout
            };
            if !ensure_read_timeout(&mut stream, sink, conn_id, &mut current_timeout, want_timeout) {
                return;
            }
            match read_more(&mut stream, &mut chunk) {
                ReadStep::Filled(n) => buf.extend_from_slice(&chunk[..n]),
                ReadStep::Eof => {
                    let reason = if buf.is_empty() {
                        "eof between requests"
                    } else {
                        "eof before request head"
                    };
                    sink.post(InboundEvent::ReaderClosed { conn_id, reason: reason.to_string() });
                    return;
                }
                ReadStep::Timeout => {
                    // An idle-timeout expiry with no partial request buffered
                    // is a silent close of a kept-alive / fresh idle
                    // connection; a timeout mid-head is the slow-loris guard.
                    let reason = if buf.is_empty() {
                        "idle"
                    } else {
                        "read timeout (head)"
                    };
                    sink.post(InboundEvent::ReaderClosed { conn_id, reason: reason.to_string() });
                    return;
                }
                ReadStep::Error(reason) => {
                    sink.post(InboundEvent::ReaderClosed { conn_id, reason });
                    return;
                }
            }
        };

        // The body read and the `Expect: 100-continue` write run under the
        // in-flight timeout — a request has started arriving.
        if !ensure_read_timeout(&mut stream, sink, conn_id, &mut current_timeout, request_timeout) {
            return;
        }

        let keep_alive = request_keeps_alive(head.version, &head.headers);

        // The request-path decision, made here (ADR-0135 §2): map the
        // method, resolve the handler against the shared route table +
        // registry, and reject — writing the canned status on this
        // reader's own socket clone — without waking the shard.
        let Some(method) = parse_http_method(&head.method) else {
            reject_and_close(&mut stream, sink, conn_id, 501, "method not implemented");
            return;
        };
        let resolution = resolve_at_reader(shared, sink, &mut route_cursor, &head.path, method);
        let (handler, dispatch_kind, streaming) = match resolution {
            ReaderResolution::Live { handler, kind, streaming } => (handler, kind, streaming),
            ReaderResolution::Dead => {
                reject_and_close(&mut stream, sink, conn_id, 503, "routed handler gone");
                return;
            }
            ReaderResolution::NoHandler => {
                reject_and_close(&mut stream, sink, conn_id, 503, "no handler registered");
                return;
            }
        };
        // ADR-0129: a websocket upgrade handshake, validated here; a
        // valid one rides the buffered path with the key attached (the
        // shard stashes it pre-dispatch). Checked before framing,
        // matching the pre-fast-path decision order — an upgrade
        // request always buffers.
        let ws_key = if header_has_token(&head.headers, "upgrade", "websocket") {
            match validate_ws_handshake(&head.headers) {
                Ok(key) => Some(key),
                Err((status, message)) => {
                    reject_and_close(&mut stream, sink, conn_id, status, message);
                    return;
                }
            }
        } else {
            None
        };
        // Whether this request will be buffered rather than streamed: a
        // websocket upgrade always buffers (line ~594 forces the
        // `else`-branch below) regardless of the `streaming` flag; a
        // non-upgrade request buffers unless it is both streaming-capable
        // and not an upgrade (the ADR-0128 streaming exemption). Shared by
        // both the framing rejects and the body-size cap below so the two
        // checks read off one determinant.
        let will_buffer = ws_key.is_some() || !streaming;
        // Framing rejects, applied on every path — upgrade included
        // (ADR-0128 + ADR-0129): a websocket handshake carries no body
        // (RFC 6455), so a smuggling shape or a lone `chunked` body on an
        // upgrade is anomalous with no length to buffer under, same as on
        // the non-upgrade path. `Invalid` (smuggling / a non-`chunked`
        // coding) always rejects; a lone `chunked` body only rejects when
        // the request will buffer.
        match (head.framing, will_buffer) {
            (BodyFraming::Invalid, _) | (BodyFraming::Chunked, true) => {
                reject_and_close(&mut stream, sink, conn_id, 411, "length required");
                return;
            }
            _ => {}
        }
        // The body-size cap applies whenever the request will be buffered.
        if will_buffer
            && let BodyFraming::Length(n) = head.framing
            && n > max_request_bytes
        {
            reject_and_close(&mut stream, sink, conn_id, 413, "request body exceeds limit");
            return;
        }

        // `Expect: 100-continue`: written only once the request is
        // accepted (every reject above returned already). The reader
        // owns a full-duplex clone of the socket, so it writes the
        // interim response inline, strictly before the body read — the
        // final response still goes out the dispatcher's `write_half`,
        // so the two writes never interleave on the shared fd.
        let expects_continue = head.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("expect") && header.value.to_ascii_lowercase().contains("100-continue")
        });
        if expects_continue && let Err(e) = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n") {
            tracing::debug!(
                target: "aether_substrate::http_server",
                conn = conn_id,
                error = %e,
                "http conn: 100-continue write failed",
            );
        }

        // Phase 2: read the body — buffered inline (the fast path: one
        // event per request, encoded here), or streamed under the
        // shard-seated session (the head round trip survives only for
        // streaming handlers). Then Phase 3: wait for the response
        // deadline. Both paths leave the reader ready to loop with the
        // over-read (pipelined) bytes in `next_buf`, or return to close.
        let next_buf = if streaming && ws_key.is_none() {
            let parsed_head = ParsedHead {
                method: head.method.clone(),
                path: head.path.clone(),
                query: head.query.clone(),
                headers: head.headers.clone(),
                framing: head.framing,
                keep_alive,
            };
            if !sink.post(InboundEvent::RequestHeadParsed { conn_id, head: parsed_head, handler }) {
                return;
            }
            // A timeout means the shard never answered (wedged / torn
            // down); a disconnect means it closed. Either way there is
            // nothing more this reader can write.
            let Ok(mode) = control_rx.recv_timeout(request_timeout) else {
                return;
            };
            let ReaderControl::Stream { credit } = mode else {
                // A bare credit / resume / upgrade as the first control
                // after a streaming head means a torn-down connection.
                return;
            };
            let leftover = buf[head.head_len..].to_vec();
            match stream_request_body(
                &mut stream,
                conn_id,
                shutdown,
                sink,
                control_rx,
                head.framing,
                leftover,
                credit,
                request_timeout,
            ) {
                StreamOutcome::Complete { next_buf } => next_buf,
                StreamOutcome::Closed => return,
            }
        } else {
            match read_buffered_body(&mut stream, conn_id, shutdown, sink, &head, &buf, max_request_bytes) {
                Some((body, next_buf)) => {
                    let payload = HttpServerRequest {
                        method,
                        path: head.path,
                        query: head.query,
                        headers: head.headers,
                        body,
                        peer_addr: shared.peer.clone(),
                    }
                    .encode_into_bytes();
                    if !sink.post(InboundEvent::RequestParsed {
                        conn_id,
                        payload,
                        handler,
                        kind: dispatch_kind,
                        method,
                        keep_alive,
                        ws_key,
                    }) {
                        return;
                    }
                    next_buf
                }
                None => return,
            }
        };

        // Phase 3: response deadline. Wait for the response bytes to
        // write ourselves (ADR-0135 §3), the streamed-response resume,
        // the handler-response timeout (`504`), or the sender being
        // dropped (the dispatcher took the close path — the connection
        // is torn down).
        let response_deadline = Instant::now() + request_timeout;
        // Wrapped so the loop can hand the pipelined carry-over to
        // exactly one consuming arm (the borrow checker cannot see that
        // every consumer exits the loop).
        let mut next_buf = Some(next_buf);
        loop {
            let now = Instant::now();
            let Some(remaining) = response_deadline.checked_duration_since(now) else {
                sink.post(InboundEvent::RequestTimedOut { conn_id });
                return;
            };
            match control_rx.recv_timeout(remaining) {
                Ok(ReaderControl::Respond { bytes, resume }) => {
                    if let Err(e) = stream.write_all(&bytes).and_then(|()| stream.flush()) {
                        tracing::debug!(
                            target: "aether_substrate::http_server",
                            conn = conn_id,
                            error = %e,
                            "http response write failed",
                        );
                        sink.post(InboundEvent::ReaderClosed { conn_id, reason: "response write failed".to_string() });
                        return;
                    }
                    if resume {
                        buf = next_buf.take().unwrap_or_default();
                        break;
                    }
                    sink.post(InboundEvent::ReaderClosed { conn_id, reason: "response written".to_string() });
                    return;
                }
                Ok(ReaderControl::Resume) => {
                    buf = next_buf.take().unwrap_or_default();
                    break;
                }
                Ok(ReaderControl::Upgrade) => {
                    // ADR-0129: the handshake was accepted (`101` written) — leave
                    // the HTTP request lifecycle for the RFC 6455 frame loop,
                    // carrying any bytes over-read past the handshake head.
                    run_ws_reader_loop(
                        &mut stream,
                        conn_id,
                        shutdown,
                        sink,
                        next_buf.take().unwrap_or_default(),
                        tuning,
                    );
                    return;
                }
                // A late credit grant for the just-finished request stream
                // (the handler's replenishment racing the RequestBodyEnd
                // drain) is benign — keep waiting out the same deadline.
                Ok(ReaderControl::Credit { .. }) => {}
                // A fresh Stream decision with no head posted means a
                // torn-down connection.
                Ok(ReaderControl::Stream { .. }) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    sink.post(InboundEvent::RequestTimedOut { conn_id });
                    return;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return;
                }
            }
        }
    }
}

/// Read a `Content-Length`-delimited body into one buffer (the buffered path).
/// Leftover bytes already past the head come first; any over-read past the body
/// (a pipelined next request) is returned as the second tuple element so
/// keep-alive framing stays intact. `None` on EOF / timeout / error / shutdown
/// (a `ReaderClosed` is posted where appropriate).
fn read_buffered_body(
    stream: &mut TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    sink: &WakeSink,
    head: &RequestHead,
    buf: &[u8],
    max_request_bytes: usize,
) -> Option<(Vec<u8>, Vec<u8>)> {
    // Defense-in-depth (the primary guard is the 413 reject above, hoisted
    // out of the `ws_key.is_none()` gate): even if some future path ever
    // reaches this read without the cap having fired, the upfront
    // allocation can never exceed it.
    let mut body: Vec<u8> = Vec::with_capacity(head.content_length.min(max_request_bytes));
    let mut next_buf: Vec<u8> = Vec::new();
    let after_head = &buf[head.head_len..];
    if after_head.len() >= head.content_length {
        body.extend_from_slice(&after_head[..head.content_length]);
        next_buf.extend_from_slice(&after_head[head.content_length..]);
    } else {
        body.extend_from_slice(after_head);
    }
    let mut chunk = [0u8; 8 * 1024];
    while body.len() < head.content_length {
        if shutdown.load(Ordering::Acquire) {
            return None;
        }
        match read_more(stream, &mut chunk) {
            ReadStep::Filled(n) => {
                let want = head.content_length - body.len();
                if n <= want {
                    body.extend_from_slice(&chunk[..n]);
                } else {
                    body.extend_from_slice(&chunk[..want]);
                    next_buf.extend_from_slice(&chunk[want..n]);
                }
            }
            ReadStep::Eof => {
                sink.post(InboundEvent::ReaderClosed { conn_id, reason: "eof mid-body".to_string() });
                return None;
            }
            ReadStep::Timeout => {
                sink.post(InboundEvent::ReaderClosed { conn_id, reason: "read timeout (body)".to_string() });
                return None;
            }
            ReadStep::Error(reason) => {
                sink.post(InboundEvent::ReaderClosed { conn_id, reason });
                return None;
            }
        }
    }
    Some((body, next_buf))
}

/// Outcome of streaming one request body to the dispatcher (ADR-0128).
enum StreamOutcome {
    /// The body completed and [`InboundEvent::RequestBodyEnd`] was posted;
    /// `next_buf` holds any over-read bytes past the body (a pipelined request).
    Complete { next_buf: Vec<u8> },
    /// The reader stopped (EOF / timeout / error / teardown); a `ReaderClosed`
    /// was posted where the stop was the reader's, or the control / wake
    /// channel disconnected.
    Closed,
}

/// Stream a request body to the dispatcher as credit-paced
/// [`InboundEvent::RequestBodyChunk`] mails, then post
/// [`InboundEvent::RequestBodyEnd`] (ADR-0128). A `Content-Length` body counts
/// down; a `chunked` body is decoded frame by frame. The reader parks on the
/// control channel when its send window is exhausted, so a fast peer backs up
/// into TCP rather than growing the handler's inbox.
#[allow(clippy::too_many_arguments)]
fn stream_request_body(
    stream: &mut TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    sink: &WakeSink,
    control_rx: &mpsc::Receiver<ReaderControl>,
    framing: BodyFraming,
    leftover: Vec<u8>,
    initial_credit: u32,
    request_timeout: Duration,
) -> StreamOutcome {
    let mut streamer = BodyStreamer {
        stream,
        conn_id,
        shutdown,
        sink,
        control_rx,
        request_timeout,
        credit: initial_credit,
        work: leftover,
        pos: 0,
    };
    let result = match framing {
        BodyFraming::Length(n) => streamer.stream_length_body(n),
        BodyFraming::Chunked => streamer.stream_chunked_body(),
        // The dispatcher never signals `Stream` for `Invalid` framing.
        BodyFraming::Invalid => Err(()),
    };
    match result {
        Ok(()) => {
            let next_buf = streamer.work[streamer.pos..].to_vec();
            if sink.post(InboundEvent::RequestBodyEnd { conn_id }) {
                StreamOutcome::Complete { next_buf }
            } else {
                StreamOutcome::Closed
            }
        }
        Err(()) => StreamOutcome::Closed,
    }
}

/// The reader-side state for streaming one request body (ADR-0128): the socket,
/// a working buffer with a cursor over unconsumed bytes, and the send-window
/// credit shared with the dispatcher through the control channel. All methods
/// return `Err(())` to mean "stop, the outcome is [`StreamOutcome::Closed`]",
/// posting a `ReaderClosed` first where the stop is a read failure.
struct BodyStreamer<'a> {
    stream: &'a mut TcpStream,
    conn_id: ConnId,
    shutdown: &'a AtomicBool,
    sink: &'a WakeSink,
    control_rx: &'a mpsc::Receiver<ReaderControl>,
    request_timeout: Duration,
    /// Un-spent send-window credit — the count of `RequestBodyChunk` mails the
    /// reader may still post before it must park for the handler's grant.
    credit: u32,
    /// Unconsumed bytes (leftover past the head, plus socket refills).
    work: Vec<u8>,
    /// Cursor into `work`; bytes before it are consumed.
    pos: usize,
}

impl BodyStreamer<'_> {
    /// Unconsumed byte count.
    fn avail(&self) -> usize {
        self.work.len() - self.pos
    }

    fn post_closed(&self, reason: &str) {
        self.sink.post(InboundEvent::ReaderClosed { conn_id: self.conn_id, reason: reason.to_string() });
    }

    /// Read one socket batch into `work`, compacting the consumed prefix first.
    /// `Err` on EOF / timeout / error / shutdown.
    fn read_socket_once(&mut self) -> Result<(), ()> {
        if self.pos > 0 {
            self.work.drain(..self.pos);
            self.pos = 0;
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(());
        }
        let mut chunk = [0u8; 8 * 1024];
        match read_more(self.stream, &mut chunk) {
            ReadStep::Filled(n) => {
                self.work.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            ReadStep::Eof => {
                self.post_closed("eof mid-body");
                Err(())
            }
            ReadStep::Timeout => {
                self.post_closed("read timeout (body)");
                Err(())
            }
            ReadStep::Error(reason) => {
                self.sink.post(InboundEvent::ReaderClosed { conn_id: self.conn_id, reason });
                Err(())
            }
        }
    }

    /// Ensure at least one unconsumed byte, reading from the socket if needed.
    fn ensure_avail(&mut self) -> Result<(), ()> {
        if self.avail() == 0 {
            self.read_socket_once()?;
        }
        Ok(())
    }

    /// Wait for a send-window credit, then deliver `body` as one inbound chunk
    /// and spend the credit (ADR-0128). An empty body is never framed. A zero
    /// window parks on the control channel until the handler grants more; a
    /// stall past `request_timeout` or a teardown returns `Err`.
    fn emit(&mut self, body: Vec<u8>) -> Result<(), ()> {
        if body.is_empty() {
            return Ok(());
        }
        while self.credit == 0 {
            if self.shutdown.load(Ordering::Acquire) {
                return Err(());
            }
            match self.control_rx.recv_timeout(self.request_timeout) {
                Ok(ReaderControl::Credit { credit } | ReaderControl::Stream { credit }) => {
                    self.credit = self.credit.saturating_add(credit);
                }
                // A bare resume / respond / upgrade mid-stream, or the
                // control channel dropped, means a torn-down connection
                // — stop.
                Ok(ReaderControl::Resume | ReaderControl::Respond { .. } | ReaderControl::Upgrade)
                | Err(mpsc::RecvTimeoutError::Disconnected) => return Err(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.post_closed("stream credit timeout");
                    return Err(());
                }
            }
        }
        if !self.sink.post(InboundEvent::RequestBodyChunk { conn_id: self.conn_id, body }) {
            return Err(());
        }
        self.credit -= 1;
        Ok(())
    }

    /// Deliver the next `remaining`-bounded run of unconsumed bytes as one
    /// chunk, refilling from the socket when empty. Returns the new remaining.
    fn deliver_slice(&mut self, remaining: usize) -> Result<usize, ()> {
        self.ensure_avail()?;
        let take = remaining.min(self.avail());
        let slice = self.work[self.pos..self.pos + take].to_vec();
        self.pos += take;
        self.emit(slice)?;
        Ok(remaining - take)
    }

    /// Stream a `Content-Length`-delimited body of `n` bytes.
    fn stream_length_body(&mut self, n: usize) -> Result<(), ()> {
        let mut remaining = n;
        while remaining > 0 {
            remaining = self.deliver_slice(remaining)?;
        }
        Ok(())
    }

    /// Stream a `chunked` transfer-encoded body, decoding it frame by frame:
    /// a hex chunk-size line, that many body bytes (delivered credit-paced),
    /// its trailing CRLF, repeated until the zero-length terminator, then any
    /// trailer up to the final blank line.
    fn stream_chunked_body(&mut self) -> Result<(), ()> {
        loop {
            let size = self.read_chunk_size()?;
            if size == 0 {
                return self.consume_trailer();
            }
            let mut remaining = size;
            while remaining > 0 {
                remaining = self.deliver_slice(remaining)?;
            }
            self.consume_crlf()?;
        }
    }

    /// Read and parse a chunk-size line: hex digits up to an optional `;ext`,
    /// terminated by CRLF. `Err` on a malformed or oversize line.
    fn read_chunk_size(&mut self) -> Result<usize, ()> {
        let line = self.read_line()?;
        let hex_end = line.iter().position(|&b| b == b';').unwrap_or(line.len());
        let text = from_utf8(&line[..hex_end]).unwrap_or("").trim();
        usize::from_str_radix(text, 16).map_err(|_| self.post_closed("malformed chunk size"))
    }

    /// Read one CRLF-terminated line (the CRLF stripped), bounded to
    /// [`MAX_CHUNK_LINE_BYTES`]. The line stays in `work` across refills so a
    /// CRLF split across two socket reads still matches.
    fn read_line(&mut self) -> Result<Vec<u8>, ()> {
        loop {
            if let Some(idx) = self.work[self.pos..].windows(2).position(|w| w == b"\r\n") {
                let line = self.work[self.pos..self.pos + idx].to_vec();
                self.pos += idx + 2;
                return Ok(line);
            }
            if self.avail() > MAX_CHUNK_LINE_BYTES {
                self.post_closed("chunk line too long");
                return Err(());
            }
            self.read_socket_once()?;
        }
    }

    /// Consume the CRLF that follows a chunk's data bytes.
    fn consume_crlf(&mut self) -> Result<(), ()> {
        while self.avail() < 2 {
            self.read_socket_once()?;
        }
        if &self.work[self.pos..self.pos + 2] == b"\r\n" {
            self.pos += 2;
            Ok(())
        } else {
            self.post_closed("malformed chunk framing");
            Err(())
        }
    }

    /// Consume trailer header lines after the terminating chunk, up to the
    /// final blank line. Trailers are ignored (our subset carries none); the
    /// count is bounded so a peer cannot stream them unboundedly.
    fn consume_trailer(&mut self) -> Result<(), ()> {
        for _ in 0..MAX_CHUNK_TRAILERS {
            if self.read_line()?.is_empty() {
                return Ok(());
            }
        }
        self.post_closed("too many chunk trailers");
        Err(())
    }
}
