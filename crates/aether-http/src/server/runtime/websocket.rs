// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

// ADR-0129 websocket support: the RFC 6455 handshake accept-key (hand-rolled
// SHA-1 + base64), the frame codec (parse / serialize / masking / continuation
// reassembly), the `101` render, and the reader-thread frame loop. All under
// `feature = "runtime"` with the rest of this module — the wasm-safe `kinds.rs`
// half is untouched.

/// RFC 6455 §1.3 handshake GUID, concatenated with the client
/// `Sec-WebSocket-Key` before the SHA-1 that forms `Sec-WebSocket-Accept`.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// RFC 6455 opcodes the cap emits / matches.
pub const OPCODE_CONTINUATION: u8 = 0x0;
pub const OPCODE_TEXT: u8 = 0x1;
pub const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

/// Compute `Sec-WebSocket-Accept` (RFC 6455 §4.2.2): base64(SHA-1(key + GUID)).
pub fn sec_websocket_accept(key: &str) -> String {
    let mut input = key.as_bytes().to_vec();
    input.extend_from_slice(WS_GUID.as_bytes());
    BASE64_STANDARD.encode(sha1(&input))
}

/// Hand-rolled SHA-1 (RFC 3174) over `data`, returning the 20-byte digest. Not
/// a general crypto primitive — a fixed, short, non-secret hash for the RFC
/// 6455 handshake (ADR-0129 §6), pinned by the RFC's own worked vector rather
/// than earning a `sha1` crate. SHA-1 is broken for collision resistance; the
/// handshake does not rely on that property (it echoes a nonce), so this use is
/// correct.
// SHA-1's working variables are single letters by the RFC 3174 spec
// (a/b/c/d/e/f/h/w/k); renaming them would obscure the transcription.
#[allow(clippy::many_single_char_names)]
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    // Pad: append 0x80, zero-fill to 56 mod 64, then the 64-bit big-endian
    // message length in bits.
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for block in msg.chunks_exact(64) {
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let j = i * 4;
            *word = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Serialize one RFC 6455 frame with FIN set: `opcode`, `payload`, optionally
/// masked with `mask`. A server→client frame is unmasked (`mask = None`, ADR-0129
/// §3); the masked path exists for the codec's own round-trip tests.
pub fn serialize_ws_frame(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0F));
    let masked_bit: u8 = if mask.is_some() {
        0x80
    } else {
        0x00
    };
    let len = payload.len();
    if len < 126 {
        out.push(masked_bit | u8::try_from(len).unwrap_or(0));
    } else if let Ok(short) = u16::try_from(len) {
        out.push(masked_bit | 0x7E);
        out.extend_from_slice(&short.to_be_bytes());
    } else {
        out.push(masked_bit | 0x7F);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    match mask {
        Some(key) => {
            out.extend_from_slice(&key);
            for (i, byte) in payload.iter().enumerate() {
                out.push(byte ^ key[i % 4]);
            }
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// Serialize an RFC 6455 close frame (opcode `0x8`): a 2-byte big-endian status
/// code followed by the UTF-8 reason, unmasked.
fn serialize_ws_close_frame(code: u16, reason: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + reason.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(reason.as_bytes());
    serialize_ws_frame(OPCODE_CLOSE, &payload, None)
}

/// One parsed RFC 6455 frame — header decoded, payload unmasked.
pub struct WsFrame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Outcome of [`parse_ws_frame`].
pub enum WsFrameParse {
    Complete {
        frame: WsFrame,
        consumed: usize,
    },
    NeedMore,
    /// A protocol violation (an unmasked client frame, a reserved bit set, or a
    /// length past the cap). Carries the RFC 6455 close code + reason the cap
    /// answers before tearing down — untrusted input never panics (ADR-0129).
    Error {
        code: u16,
        reason: &'static str,
    },
}

/// Parse one frame from the front of `buf`. A client→server frame MUST be
/// masked (RFC 6455 §5.1); `max_payload` bounds one frame's payload so an
/// oversize length is a protocol error, not an allocation.
pub fn parse_ws_frame(buf: &[u8], max_payload: usize) -> WsFrameParse {
    if buf.len() < 2 {
        return WsFrameParse::NeedMore;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    if b0 & 0x70 != 0 {
        return WsFrameParse::Error { code: 1002, reason: "reserved bits set" };
    }
    let fin = b0 & 0x80 != 0;
    let opcode = b0 & 0x0F;
    if b1 & 0x80 == 0 {
        return WsFrameParse::Error { code: 1002, reason: "client frame not masked" };
    }
    let len7 = usize::from(b1 & 0x7F);
    let mut cursor = 2;
    let payload_len = match len7 {
        126 => {
            if buf.len() < cursor + 2 {
                return WsFrameParse::NeedMore;
            }
            let n = usize::from(u16::from_be_bytes([buf[cursor], buf[cursor + 1]]));
            cursor += 2;
            n
        }
        127 => {
            if buf.len() < cursor + 8 {
                return WsFrameParse::NeedMore;
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&buf[cursor..cursor + 8]);
            cursor += 8;
            match usize::try_from(u64::from_be_bytes(arr)) {
                Ok(n) => n,
                Err(_) => {
                    return WsFrameParse::Error { code: 1009, reason: "frame too large" };
                }
            }
        }
        n => n,
    };
    if payload_len > max_payload {
        return WsFrameParse::Error { code: 1009, reason: "frame too large" };
    }
    if buf.len() < cursor + 4 + payload_len {
        return WsFrameParse::NeedMore;
    }
    let mask = [buf[cursor], buf[cursor + 1], buf[cursor + 2], buf[cursor + 3]];
    cursor += 4;
    let mut payload = Vec::with_capacity(payload_len);
    for (i, &byte) in buf[cursor..cursor + payload_len].iter().enumerate() {
        payload.push(byte ^ mask[i % 4]);
    }
    cursor += payload_len;
    WsFrameParse::Complete { frame: WsFrame { fin, opcode, payload }, consumed: cursor }
}

/// Decode a close frame payload: an optional 2-byte big-endian status code
/// followed by a UTF-8 reason. An absent code is reported as `1005` (RFC 6455
/// "no status received").
fn parse_ws_close_payload(payload: &[u8]) -> (u16, String) {
    if payload.len() < 2 {
        return (1005, String::new());
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    (code, String::from_utf8_lossy(&payload[2..]).into_owned())
}

/// Render the RFC 6455 `101 Switching Protocols` handshake response (ADR-0129
/// §1): the mandated `Upgrade` / `Connection` / `Sec-WebSocket-Accept`, the
/// handler's optional negotiated subprotocol, and any extra handler headers
/// (the three cap-owned handshake headers are stripped so a handler cannot
/// double them).
fn render_ws_accept(key: &str, accept: &WebSocketAccept) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut head = String::from("HTTP/1.1 101 Switching Protocols\r\n");
    head.push_str("Upgrade: websocket\r\n");
    head.push_str("Connection: Upgrade\r\n");
    let _ = write!(head, "Sec-WebSocket-Accept: {}\r\n", sec_websocket_accept(key));
    if let Some(protocol) = &accept.subprotocol {
        let _ = write!(head, "Sec-WebSocket-Protocol: {protocol}\r\n");
    }
    for header in &accept.headers {
        if header.name.eq_ignore_ascii_case("upgrade")
            || header.name.eq_ignore_ascii_case("connection")
            || header.name.eq_ignore_ascii_case("sec-websocket-accept")
        {
            continue;
        }
        let _ = write!(head, "{}: {}\r\n", header.name, header.value);
    }
    head.push_str("\r\n");
    head.into_bytes()
}

/// Whether the reader's frame loop keeps running or stops (peer close /
/// protocol error / receiver gone).
enum WsLoop {
    Continue,
    Stop,
}

/// Act on one parsed inbound frame (ADR-0129 §4/§5), driving continuation
/// reassembly and control-frame handling. Application messages / pings / peer
/// closes post an [`InboundEvent`]; the cap (dispatcher + writer thread) does
/// all the writing — the reader only reads and reports.
#[allow(clippy::too_many_arguments)]
fn handle_ws_frame(
    frame: WsFrame,
    conn_id: ConnId,
    sink: &WakeSink,
    msg: &mut Vec<u8>,
    msg_binary: &mut bool,
    fragmenting: &mut bool,
    max_message_bytes: usize,
) -> WsLoop {
    let protocol_close = |code: u16, reason: &str| {
        sink.post(InboundEvent::WebSocketClose { conn_id, code, reason: reason.to_string() });
        WsLoop::Stop
    };
    match frame.opcode {
        OPCODE_PING => {
            if !frame.fin {
                return protocol_close(1002, "fragmented control frame");
            }
            if sink.post(InboundEvent::WebSocketPing { conn_id, payload: frame.payload }) {
                WsLoop::Continue
            } else {
                WsLoop::Stop
            }
        }
        // A pong (peer keepalive answer) is transparent — nothing to do.
        OPCODE_PONG => WsLoop::Continue,
        OPCODE_CLOSE => {
            let (code, reason) = parse_ws_close_payload(&frame.payload);
            sink.post(InboundEvent::WebSocketClose { conn_id, code, reason });
            WsLoop::Stop
        }
        OPCODE_TEXT | OPCODE_BINARY => {
            if *fragmenting {
                return protocol_close(1002, "interleaved data frame");
            }
            let binary = frame.opcode == OPCODE_BINARY;
            if frame.fin {
                if sink.post(InboundEvent::WebSocketMessage { conn_id, binary, data: frame.payload }) {
                    WsLoop::Continue
                } else {
                    WsLoop::Stop
                }
            } else {
                *msg = frame.payload;
                *msg_binary = binary;
                *fragmenting = true;
                WsLoop::Continue
            }
        }
        OPCODE_CONTINUATION => {
            if !*fragmenting {
                return protocol_close(1002, "unexpected continuation frame");
            }
            if msg.len() + frame.payload.len() > max_message_bytes {
                return protocol_close(1009, "message too large");
            }
            msg.extend_from_slice(&frame.payload);
            if frame.fin {
                *fragmenting = false;
                let data = mem::take(msg);
                if sink.post(InboundEvent::WebSocketMessage { conn_id, binary: *msg_binary, data }) {
                    WsLoop::Continue
                } else {
                    WsLoop::Stop
                }
            } else {
                WsLoop::Continue
            }
        }
        _ => protocol_close(1002, "unknown opcode"),
    }
}

/// The websocket frame loop (ADR-0129 §4): once upgraded, the reader reads RFC
/// 6455 frames under the idle deadline, reassembles continuation frames into
/// complete messages, and posts each application message / ping / peer close as
/// an [`InboundEvent`]. It writes nothing — every outbound byte (pong, close
/// echo, and the handler's messages) goes through the single writer thread, so
/// no two writers race on the shared fd. `leftover` carries any bytes over-read
/// past the handshake head. Exits on peer close / protocol error / EOF / idle
/// timeout, posting a [`InboundEvent::WebSocketClose`] so the dispatcher tears
/// the connection down.
pub fn run_ws_reader_loop(
    stream: &mut TcpStream,
    conn_id: ConnId,
    shutdown: &AtomicBool,
    sink: &WakeSink,
    leftover: Vec<u8>,
    tuning: ReaderTuning,
) {
    let ReaderTuning { ws_idle_timeout, ws_max_message_bytes, .. } = tuning;
    // An idle websocket is normal — read under the (longer) idle deadline. If the
    // setsockopt itself fails, the reader has no idle bound to enforce; close
    // rather than enter the frame loop with an unbounded blocking read.
    if stream.set_read_timeout(Some(ws_idle_timeout)).is_err() {
        sink.post(InboundEvent::WebSocketClose {
            conn_id,
            code: 1011,
            reason: "failed to arm websocket idle timeout".to_string(),
        });
        return;
    }
    let mut buf = leftover;
    let mut msg: Vec<u8> = Vec::new();
    let mut msg_binary = false;
    let mut fragmenting = false;
    let mut chunk = [0u8; 8 * 1024];

    loop {
        // Drain every whole frame the buffer already holds before reading more.
        loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match parse_ws_frame(&buf, ws_max_message_bytes) {
                WsFrameParse::Complete { frame, consumed } => {
                    buf.drain(..consumed);
                    if matches!(
                        handle_ws_frame(
                            frame,
                            conn_id,
                            sink,
                            &mut msg,
                            &mut msg_binary,
                            &mut fragmenting,
                            ws_max_message_bytes,
                        ),
                        WsLoop::Stop
                    ) {
                        return;
                    }
                }
                WsFrameParse::NeedMore => break,
                WsFrameParse::Error { code, reason } => {
                    sink.post(InboundEvent::WebSocketClose { conn_id, code, reason: reason.to_string() });
                    return;
                }
            }
        }
        match read_more(stream, &mut chunk) {
            ReadStep::Filled(n) => buf.extend_from_slice(&chunk[..n]),
            ReadStep::Eof => {
                sink.post(InboundEvent::WebSocketClose {
                    conn_id,
                    code: 1006,
                    reason: "peer disconnected".to_string(),
                });
                return;
            }
            ReadStep::Timeout => {
                sink.post(InboundEvent::WebSocketClose { conn_id, code: 1001, reason: "idle timeout".to_string() });
                return;
            }
            ReadStep::Error(reason) => {
                sink.post(InboundEvent::WebSocketClose { conn_id, code: 1006, reason });
                return;
            }
        }
    }
}

impl HttpShardState {
    /// Accept a websocket upgrade (ADR-0129 §1): the handler replied
    /// `WebSocketAccept` to a stashed-key upgrade request. Compute
    /// `Sec-WebSocket-Accept`, write the `101 Switching Protocols` head
    /// (inline, strictly before the writer thread starts, so the two never
    /// interleave on the shared fd), spawn the ADR-0128 writer thread for
    /// outbound frames, grant the handler its initial outbound credit window,
    /// and flip the reader into the RFC 6455 frame loop. `correlation` is the
    /// handshake request's in-flight key.
    pub fn accept_websocket(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        correlation: u64,
        conn_id: ConnId,
        handler: MailboxId,
        accept: &WebSocketAccept,
    ) {
        self.in_flight.remove(&correlation);
        let Some(key) = self.connections.get_mut(&conn_id).and_then(|conn| conn.ws_pending_key.take()) else {
            // A `WebSocketAccept` with no stashed key — the handler replied it
            // to a non-upgrade request. Cap-level error; the parked reader
            // writes the canned status and exits (ADR-0135 §3).
            self.respond_and_finish(conn_id, render_status_response(500, "websocket accept without upgrade"), false);
            return;
        };
        let head = render_ws_accept(&key, accept);
        self.write_raw_to(conn_id, &head);

        let Some(conn) = self.connections.get(&conn_id) else {
            return;
        };
        let write_half = match conn.write_half.try_clone() {
            Ok(half) => half,
            Err(e) => {
                tracing::warn!(
                    target: "aether_http::server",
                    conn = conn_id,
                    error = %e,
                    "http websocket: writer clone failed; closing",
                );
                self.close_connection(conn_id, "websocket writer clone failed");
                return;
            }
        };

        let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let window = self.response_stream_window.max(1);
        let (tx, rx) = mpsc::sync_channel::<WriterMsg>(window as usize);
        let sink = self.wake_sink();
        // The writer's idle deadline is the websocket idle timeout: an upgraded
        // socket with nothing to write for minutes is normal, unlike a stalled
        // response stream.
        let idle_deadline = self.ws_idle_timeout;

        #[allow(clippy::disallowed_methods)]
        let writer_thread =
            match thread::Builder::new().name(format!("aether-http-writer-{conn_id}")).spawn(move || {
                run_writer_loop(write_half, stream_id, &rx, &sink, idle_deadline);
            }) {
                Ok(thread) => thread,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_http::server",
                        conn = conn_id,
                        error = %e,
                        "http websocket: writer thread spawn failed; closing",
                    );
                    self.close_connection(conn_id, "websocket writer spawn failed");
                    return;
                }
            };

        self.streams.insert(
            stream_id,
            StreamState {
                conn_id,
                handler,
                tx,
                writer_thread: Some(writer_thread),
                credit_outstanding: window,
                pending_end: false,
                // A websocket's `StreamFinished` (the close handshake's final
                // write, a protocol error, or an idle timeout) always tears
                // the connection down — there is no keep-alive resume once a
                // connection has upgraded.
                keep_alive: false,
            },
        );
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            conn.websocket = Some(WsConn { handler, stream_id });
        }
        // Grant the initial outbound window; the handler learns its `stream_id`
        // from this first credit mail (ADR-0128 / ADR-0129 §3).
        self.send_stream_credit(ctx, stream_id, window);
        // Flip the parked reader into the frame loop.
        self.signal_reader(conn_id, ReaderControl::Upgrade);
        tracing::debug!(
            target: "aether_http::server",
            conn = conn_id,
            stream = stream_id,
            window,
            "http websocket upgraded",
        );
    }

    /// The websocket-upgraded connection's dispatch handler + `stream_id`
    /// (ADR-0132), or `None` if `conn_id` names no such connection — the
    /// shared lookup behind every ws dispatch/close/send site.
    fn ws_target(&self, conn_id: ConnId) -> Option<(MailboxId, u64)> {
        self.connections.get(&conn_id).and_then(|conn| conn.websocket.as_ref()).map(|ws| (ws.handler, ws.stream_id))
    }

    /// Deliver one reassembled inbound websocket message to the handler
    /// (ADR-0129 §4) as a `WebSocketMessage` on its own fresh causal root,
    /// stamped with the connection's `stream_id` (ADR-0132) so the handler
    /// knows which socket it arrived on and can answer — or push later — by
    /// naming that id.
    pub fn dispatch_ws_message(&mut self, ctx: &mut NativeCtx<'_>, conn_id: ConnId, binary: bool, data: Vec<u8>) {
        let Some((handler, stream_id)) = self.ws_target(conn_id) else {
            return;
        };
        let payload = WebSocketMessage { stream_id, binary, data }.encode_into_bytes();
        let _ = ctx.send_envelope_detached(handler, <WebSocketMessage as Kind>::ID, &payload);
    }

    /// Report a peer-initiated websocket close to the handler (ADR-0129 §5) as
    /// a `WebSocketClose` on its own fresh root — the inbound-close analog of
    /// [`Self::dispatch_ws_message`], so the handler observes the disconnect.
    pub fn report_ws_close(&mut self, ctx: &mut NativeCtx<'_>, conn_id: ConnId, code: u16, reason: &str) {
        let Some((handler, stream_id)) = self.ws_target(conn_id) else {
            return;
        };
        let payload = WebSocketClose { stream_id, code, reason: reason.to_string() }.encode_into_bytes();
        let _ = ctx.send_envelope_detached(handler, <WebSocketClose as Kind>::ID, &payload);
    }

    /// Frame an outbound application message and hand it to the connection's
    /// writer thread (ADR-0129 §3). Mirrors [`Self::push_chunk`]: a message
    /// arriving with zero outstanding credit is an over-window flood and tears
    /// the connection down; otherwise it spends one credit and queues the
    /// serialized frame.
    pub fn send_ws_message(&mut self, conn_id: ConnId, binary: bool, data: &[u8]) {
        let Some((_, stream_id)) = self.ws_target(conn_id) else {
            return;
        };
        let opcode = if binary {
            OPCODE_BINARY
        } else {
            OPCODE_TEXT
        };
        let frame = serialize_ws_frame(opcode, data, None);
        let Some(has_credit) = self.streams.get(&stream_id).map(|stream| stream.credit_outstanding > 0) else {
            return;
        };
        if !has_credit {
            self.teardown_stream(stream_id);
            self.close_connection(conn_id, "websocket send credit exceeded");
            return;
        }
        let send_result = {
            let stream = self.streams.get_mut(&stream_id).expect("stream present under the same borrow");
            stream.credit_outstanding -= 1;
            stream.tx.try_send(WriterMsg::WsData(frame))
        };
        if send_result.is_err() {
            self.teardown_stream(stream_id);
            self.close_connection(conn_id, "websocket writer unavailable");
        }
    }

    /// Queue a cap-owned pong answering an inbound ping (ADR-0129 §5) on the
    /// connection's writer thread. Uncredited and best-effort — a full writer
    /// channel drops the pong rather than blocking or tearing down.
    pub fn send_ws_pong(&mut self, conn_id: ConnId, payload: &[u8]) {
        let Some((_, stream_id)) = self.ws_target(conn_id) else {
            return;
        };
        let frame = serialize_ws_frame(OPCODE_PONG, payload, None);
        if let Some(stream) = self.streams.get(&stream_id) {
            let _ = stream.tx.try_send(WriterMsg::WsControl(frame));
        }
    }

    /// Initiate / complete a websocket close (ADR-0129 §5): queue a close frame
    /// on the writer as its final write (the writer posts `StreamFinished`,
    /// which tears the connection down). Serves both a handler-initiated close
    /// and the echo of a peer close frame.
    pub fn send_ws_close(&mut self, conn_id: ConnId, code: u16, reason: &str) {
        let Some((_, stream_id)) = self.ws_target(conn_id) else {
            return;
        };
        let frame = serialize_ws_close_frame(code, reason);
        if let Some(stream) = self.streams.get(&stream_id) {
            if stream.tx.try_send(WriterMsg::WsClose(frame)).is_err() {
                // Writer gone — tear down directly.
                self.teardown_stream(stream_id);
                self.close_connection(conn_id, "websocket close write failed");
            }
        } else {
            self.close_connection(conn_id, "websocket already closed");
        }
    }
}
