// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

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
/// `is_head` emits the full head — including the `Content-Length` the
/// body would have had — but appends no body bytes (HEAD semantics: a
/// message with no message body). `keep_alive` renders `Connection:
/// keep-alive` (the connection is held for the next request) vs
/// `Connection: close`.
pub fn render_handler_response(response: &HttpServerResponse, is_head: bool, keep_alive: bool) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason_phrase(response.status));
    for header in &response.headers {
        if is_cap_owned_header(&header.name) {
            continue;
        }
        let _ = write!(head, "{}: {}\r\n", header.name, header.value);
    }
    let _ = write!(head, "Content-Length: {}\r\n", response.body.len());
    let _ = write!(head, "Date: {}\r\n", http_date(SystemTime::now()));
    if keep_alive {
        head.push_str("Connection: keep-alive\r\n\r\n");
    } else {
        head.push_str("Connection: close\r\n\r\n");
    }
    let mut out = head.into_bytes();
    if !is_head {
        out.extend_from_slice(&response.body);
    }
    out
}

/// Render a canned status response with a plain-text body.
pub fn render_status_response(status: u16, message: &str) -> Vec<u8> {
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

/// Render the response head for a streamed response (ADR-0128): the status
/// line, the handler's headers (cap-owned ones stripped), `Transfer-Encoding:
/// chunked` in place of `Content-Length`, and `Date` / `Connection`.
/// `keep_alive` renders `Connection: keep-alive` (the connection is held for
/// the next request once the stream ends) vs `Connection: close`, exactly
/// like [`render_handler_response`]'s buffered decision.
pub fn render_stream_head(open: &HttpResponseStreamOpen, keep_alive: bool) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut head = format!("HTTP/1.1 {} {}\r\n", open.status, reason_phrase(open.status));
    for header in &open.headers {
        if is_cap_owned_header(&header.name) {
            continue;
        }
        let _ = write!(head, "{}: {}\r\n", header.name, header.value);
    }
    head.push_str("Transfer-Encoding: chunked\r\n");
    let _ = write!(head, "Date: {}\r\n", http_date(SystemTime::now()));
    if keep_alive {
        head.push_str("Connection: keep-alive\r\n\r\n");
    } else {
        head.push_str("Connection: close\r\n\r\n");
    }
    head.into_bytes()
}

/// Per-connection response-stream writer thread body (ADR-0128). Drains the
/// bounded hand-off channel, framing each chunk as chunked transfer-encoding
/// and writing the terminating chunk on `End`. TCP backpressure blocks this
/// thread on the socket write, never the dispatcher. A `recv_timeout` past
/// `idle_deadline` is the idle-write / no-progress deadline: a handler that
/// stalled mid-stream tears the stream down.
pub fn run_writer_loop(
    mut write_half: TcpStream,
    stream_id: u64,
    rx: &mpsc::Receiver<WriterMsg>,
    sink: &WakeSink,
    idle_deadline: Duration,
) {
    loop {
        match rx.recv_timeout(idle_deadline) {
            Ok(WriterMsg::Chunk(body)) => {
                if write_chunk(&mut write_half, &body).is_err() {
                    // Peer disconnected / socket error mid-stream.
                    sink.post(InboundEvent::StreamFinished { stream_id });
                    return;
                }
                // One window slot freed — let the dispatcher replenish credit.
                if !sink.post(InboundEvent::StreamSlotFreed { stream_id }) {
                    return;
                }
            }
            Ok(WriterMsg::End) => {
                let _ = write_terminator(&mut write_half);
                sink.post(InboundEvent::StreamFinished { stream_id });
                return;
            }
            Ok(WriterMsg::WsData(frame)) => {
                // A pre-serialized websocket data frame (ADR-0129): write it
                // verbatim, then free a credit slot like a response chunk.
                if write_all_flush(&mut write_half, &frame).is_err() {
                    sink.post(InboundEvent::StreamFinished { stream_id });
                    return;
                }
                if !sink.post(InboundEvent::StreamSlotFreed { stream_id }) {
                    return;
                }
            }
            Ok(WriterMsg::WsControl(frame)) => {
                // A cap-owned pong (ADR-0129 §5): verbatim, uncredited — no
                // slot-freed event, so it never inflates the handler's window.
                if write_all_flush(&mut write_half, &frame).is_err() {
                    sink.post(InboundEvent::StreamFinished { stream_id });
                    return;
                }
            }
            Ok(WriterMsg::WsClose(frame)) => {
                // The close handshake's final write (ADR-0129 §5): verbatim,
                // then finish so the dispatcher tears the connection down.
                let _ = write_all_flush(&mut write_half, &frame);
                sink.post(InboundEvent::StreamFinished { stream_id });
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle-write deadline (ADR-0128 §4): the handler stalled.
                sink.post(InboundEvent::StreamFinished { stream_id });
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The dispatcher dropped the sender (stream torn down
                // elsewhere) — nothing more to write.
                return;
            }
        }
    }
}

/// Frame one body piece as a chunked transfer-encoding chunk (`hexlen CRLF
/// body CRLF`) and flush it. An empty body is skipped rather than framed — a
/// zero-length chunk is the transfer terminator, which only `End` may write.
fn write_chunk(write_half: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    let header = format!("{:x}\r\n", body.len());
    write_half.write_all(header.as_bytes())?;
    write_half.write_all(body)?;
    write_half.write_all(b"\r\n")?;
    write_half.flush()
}

/// Write the chunked transfer terminator (the zero-length chunk) and flush.
fn write_terminator(write_half: &mut TcpStream) -> io::Result<()> {
    write_half.write_all(b"0\r\n\r\n")?;
    write_half.flush()
}

/// Write `bytes` verbatim and flush — a pre-serialized websocket frame
/// (ADR-0129), with no chunked-transfer framing layered on.
fn write_all_flush(write_half: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    write_half.write_all(bytes)?;
    write_half.flush()
}

/// Map a raw HTTP method token to the typed [`HttpMethod`]; `None` for a
/// non-enumerated verb (answered `501` before any dispatch).
pub fn parse_http_method(method: &str) -> Option<HttpMethod> {
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
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        426 => "Upgrade Required",
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
pub fn http_date(now: SystemTime) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
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
    let month = if mp < 10 {
        mp + 3
    } else {
        mp - 9
    };
    let year = if month <= 2 {
        year + 1
    } else {
        year
    };
    let weekday_name = WEEKDAYS[usize::try_from(weekday).unwrap_or(0)];
    let month_name = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
    format!("{weekday_name}, {day:02} {month_name} {year:04} {hour:02}:{minute:02}:{second:02} GMT")
}
