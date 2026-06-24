//! The `aether.http.server` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `HttpServerCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` in the parent reaches
//! the state, ctx, and helper types through the single `use runtime::*` glob.
//!
//! Holds the state-bearing `HttpServerCapabilityState` (the 11 fields: the
//! listener port, the accept thread, the per-connection table, the internal
//! mpsc, and the in-flight correlation table), its helper-method impl, the
//! reader/accept sidecar free functions, and the parse/render support types.
//! The reader and accept threads capture only `Arc` / channel / id clones
//! built from locals in `init` (parent) and `spawn_reader_for_peer` (here) —
//! never the cap struct — so the field-home move into this state does not
//! change what the threads capture.

// Parent-level items this module names.
use super::{HttpInboundReady, Settled};

pub use std::collections::HashMap;
pub use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::sync::mpsc;
pub use std::thread;
pub use std::time::Duration;

pub use aether_data::{Kind, KindId, MailboxId};
pub use aether_substrate::actor::native::envelope::Envelope;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::mailer::Mailer;

// The parent `#[actor] impl` writes the `502` reply path, so it names
// `HttpServerResponse`; the rest of the kind vocabulary is used only here.
pub use crate::http::kinds::HttpServerResponse;
use crate::http::kinds::{HttpHeader, HttpMethod, HttpServerRequest};

use aether_substrate::Mail;
use std::io::{self, Read, Write};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-connection identifier, monotonic within this cap. Distinct from
/// the OS-level peer addr (one peer may reconnect; ids stay unique for
/// the cap's lifetime).
pub type ConnId = u64;

/// Header-array size for the inbound parse (doubles as the header-count
/// cap: a request with more headers is answered `431`). ADR-0108 §6.
const MAX_HEADER_COUNT: usize = 64;

/// One parsed inbound HTTP/1.1 request the reader hands to the
/// dispatcher. The method stays a raw `String` here; the dispatcher
/// maps it to [`HttpMethod`] and answers `501` for a non-enumerated
/// verb before any dispatch.
pub struct ParsedRequest {
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
pub enum InboundEvent {
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
pub struct ConnState {
    peer: SocketAddr,
    /// Dispatcher's half — used to write the HTTP/1.1 response.
    pub(super) write_half: TcpStream,
    /// Reader thread's shutdown flag. Cap flips it + shuts down the
    /// socket to wake the blocked `read()`.
    pub(super) shutdown: Arc<AtomicBool>,
    /// Reader thread handle. Joined in `unwire`, detached on close.
    pub(super) reader_thread: Option<JoinHandle<()>>,
}

/// Bookkeeping for one in-flight request. Looked up by the dispatch's
/// auto-minted `correlation_id` (== the dispatched envelope's
/// `MailId.correlation_id`, which is also the root id since the cap
/// always dispatches via `send_envelope_as_root`).
#[derive(Copy, Clone)]
pub struct PendingRequest {
    pub(super) conn_id: ConnId,
}

/// Wake sink shared with the accept + reader sidecar threads: push an
/// [`InboundEvent`] over the mpsc, then fire an [`HttpInboundReady`]
/// wake mail at the cap so the dispatcher drains.
pub struct WakeSink {
    pub(super) inbound_tx: mpsc::Sender<InboundEvent>,
    pub(super) mailer: Arc<Mailer>,
    pub(super) self_id: MailboxId,
    pub(super) wake_kind: KindId,
}

impl WakeSink {
    /// Post one event + wake. Returns `false` when the receiver is gone
    /// (the cap tore down) so the caller stops.
    pub(super) fn post(&self, event: InboundEvent) -> bool {
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

/// `aether.http.server` runtime state. Owns one TCP listener + per-connection
/// state + the in-flight correlation table. The dispatcher holds this as the
/// cap's state and routes envelopes through the macro-emitted `Dispatch`
/// impl; the addressing identity is the distinct ZST `HttpServerCapability`.
/// Living in this private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct HttpServerCapabilityState {
    pub(super) handler_mailbox: String,
    pub(super) max_request_bytes: usize,
    pub(super) max_header_bytes: usize,
    pub(super) request_timeout: Duration,
    pub(super) self_mailbox: MailboxId,
    /// Cached `Arc<Mailer>` so the dispatcher can fire wake mails into
    /// the cap, resolve the handler mailbox by name at dispatch time,
    /// and subscribe to settlement. The cap is single-threaded
    /// post-ADR-0038 so direct storage is fine.
    pub(super) mailer: Arc<Mailer>,
    pub(super) listener_port: u16,
    pub(super) accept_shutdown: Arc<AtomicBool>,
    pub(super) accept_thread: Option<JoinHandle<()>>,
    pub(super) inbound_rx: mpsc::Receiver<InboundEvent>,
    pub(super) inbound_tx: mpsc::Sender<InboundEvent>,
    pub(super) connections: HashMap<ConnId, ConnState>,
    pub(super) next_conn_id: ConnId,
    /// Dispatch-correlation → open response socket. Populated on
    /// dispatch; cleared on reply, settlement, timeout, or close.
    pub(super) in_flight: HashMap<u64, PendingRequest>,
}

impl HttpServerCapabilityState {
    /// Allocate a fresh `ConnId`, store the connection's write half, and
    /// spin a reader thread for the read half.
    pub(super) fn spawn_reader_for_peer(&mut self, stream: TcpStream, peer: SocketAddr) {
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
    pub(super) fn dispatch_request(
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
        let mail_id = ctx.send_envelope_as_root(handler, <HttpServerRequest as Kind>::ID, &payload);
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
    pub(super) fn write_handler_response(
        &mut self,
        conn_id: ConnId,
        response: &HttpServerResponse,
    ) {
        let bytes = render_handler_response(response);
        self.write_raw_to(conn_id, &bytes);
    }

    /// Format + write a canned status response (the cap's own
    /// `413` / `431` / `501` / `502` / `503` / `504`).
    pub(super) fn write_status_response(&mut self, conn_id: ConnId, status: u16, message: &str) {
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

    pub(super) fn close_connection(&mut self, conn_id: ConnId, reason: &str) {
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
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
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
    format!("{weekday_name}, {day:02} {month_name} {year:04} {hour:02}:{minute:02}:{second:02} GMT")
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
