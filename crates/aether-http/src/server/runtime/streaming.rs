// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

impl HttpShardState {
    /// Open an inbound request stream (ADR-0128): mint a `stream_id`, record
    /// the stream, send the handler an `HttpRequestStreamOpen`, and seed the
    /// reader's send window. The handler learns its `stream_id` here and paces
    /// the cap by mailing `HttpRequestCredit`.
    pub fn start_request_stream(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        handler: MailboxId,
        method: HttpMethod,
        head: ParsedHead,
    ) {
        let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let window = self.request_stream_window.max(1);
        self.request_streams
            .insert(stream_id, RequestStreamState { conn_id, handler, method, keep_alive: head.keep_alive });
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            conn.active_stream = Some(stream_id);
        }
        let payload =
            HttpRequestStreamOpen { stream_id, method, path: head.path, query: head.query, headers: head.headers }
                .encode_into_bytes();
        let _ = ctx.send_envelope_detached(handler, <HttpRequestStreamOpen as Kind>::ID, &payload);
        self.signal_reader(conn_id, ReaderControl::Stream { credit: window });
        tracing::debug!(
            target: "aether_http::server",
            conn = conn_id,
            stream = stream_id,
            window,
            "http request stream opened",
        );
    }

    /// Forward one inbound body piece to the handler as an `HttpRequestChunk`
    /// on the connection's active stream (ADR-0128). A missing stream (the
    /// connection closed, or the stream already ended) drops the chunk.
    pub fn forward_request_chunk(&mut self, ctx: &mut NativeCtx<'_>, conn_id: ConnId, body: Vec<u8>) {
        let Some(stream_id) = self.connections.get(&conn_id).and_then(|c| c.active_stream) else {
            return;
        };
        let Some(handler) = self.request_streams.get(&stream_id).map(|s| s.handler) else {
            return;
        };
        let payload = HttpRequestChunk { stream_id, body }.encode_into_bytes();
        let _ = ctx.send_envelope_detached(handler, <HttpRequestChunk as Kind>::ID, &payload);
    }

    /// Finish an inbound request stream (ADR-0128): send the handler an
    /// `HttpRequestStreamEnd` and record it as the in-flight request whose
    /// buffered `HttpServerResponse` reply, riding the terminator's envelope
    /// correlation, the reply-interception path writes back — so a streamed
    /// upload answers with one ordinary response and the settlement safety net
    /// still `502`s a handler that drops without replying.
    pub fn end_request_stream(&mut self, ctx: &mut NativeCtx<'_>, conn_id: ConnId) {
        let Some(stream_id) = self.connections.get_mut(&conn_id).and_then(|c| c.active_stream.take()) else {
            return;
        };
        let Some(stream) = self.request_streams.remove(&stream_id) else {
            return;
        };
        let payload = HttpRequestStreamEnd { stream_id }.encode_into_bytes();
        let mail_id = ctx.send_envelope_detached(stream.handler, <HttpRequestStreamEnd as Kind>::ID, &payload);
        self.subscribe_settlement(mail_id);
        self.in_flight.insert(
            mail_id.correlation_id,
            PendingRequest { conn_id, method: stream.method, keep_alive: stream.keep_alive, handler: stream.handler },
        );
    }

    /// Replenish a streaming reader's send window by `credit` on the handler's
    /// grant (ADR-0128). A grant for an ended / unknown stream is a no-op.
    pub fn replenish_reader_credit(&mut self, stream_id: u64, credit: u32) {
        let Some(conn_id) = self.request_streams.get(&stream_id).map(|s| s.conn_id) else {
            return;
        };
        self.signal_reader(conn_id, ReaderControl::Credit { credit });
    }

    /// Promote a buffered request to a response stream (ADR-0128): drop its
    /// in-flight entry so the settlement safety net no longer trips `502` on
    /// this chain, write the chunked response head, spawn the per-connection
    /// writer thread, and grant the handler its initial credit window.
    ///
    /// `correlation` is the request's dispatch correlation id — the key of the
    /// in-flight entry this reply belongs to, and nothing more. The stream it
    /// opens gets its own id from [`HttpShardState::next_stream_id`], the same
    /// counter `accept_websocket` mints from: a correlation id is minted per
    /// sender (`MailId` is the pair `{sender, correlation_id}`), so the bare
    /// value is unique only within one sender and cannot identify a stream
    /// (ADR-0128 §2 as amended 2026-07-20; issue 3730).
    pub fn open_stream(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        correlation: u64,
        conn_id: ConnId,
        open: &HttpResponseStreamOpen,
    ) {
        // The stream chain settles here; the request is no longer in-flight
        // (ADR-0128 §4), so `on_settled` won't `502` it. The handler is carried
        // out of the in-flight dispatch record — whichever mailbox actually
        // replied, be it the registrant of the matched `/` catch-all route or a
        // more specific registered route (ADR-0131) — so credit grants reach the
        // real replier regardless of dispatch path. A missing in-flight entry
        // leaves the sentinel and credit sends drop harmlessly, the pre-store
        // no-op behavior.
        let (keep_alive, handler) = self
            .in_flight
            .remove(&correlation)
            .map_or((false, MailboxId(0)), |pending| (pending.keep_alive, pending.handler));
        let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let head = render_stream_head(open, keep_alive);
        self.write_raw_to(conn_id, &head);

        let Some(conn) = self.connections.get(&conn_id) else {
            // Connection already gone — nothing to stream to.
            return;
        };
        let write_half = match conn.write_half.try_clone() {
            Ok(half) => half,
            Err(e) => {
                tracing::warn!(
                    target: "aether_http::server",
                    conn = conn_id,
                    error = %e,
                    "http stream: writer clone failed; closing",
                );
                self.close_connection(conn_id, "stream writer clone failed");
                return;
            }
        };

        let window = self.response_stream_window.max(1);
        let (tx, rx) = mpsc::sync_channel::<WriterMsg>(window as usize);
        let sink = self.wake_sink();
        let idle_deadline = self.request_timeout;

        // Per-connection writer below the mail layer, mirroring the reader
        // sidecar — it owns only the socket write, never the cap state.
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
                        "http stream: writer thread spawn failed; closing",
                    );
                    self.close_connection(conn_id, "stream writer spawn failed");
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
                keep_alive,
            },
        );
        // Grant the initial credit window — the handler learns its
        // `stream_id` from this first credit mail.
        self.send_stream_credit(ctx, stream_id, window);
        tracing::debug!(
            target: "aether_http::server",
            conn = conn_id,
            stream = stream_id,
            window,
            "http response stream opened",
        );
    }

    /// Accept one body chunk from the handler (ADR-0128): a chunk arriving
    /// with zero outstanding credit is an over-window flood and tears the
    /// stream down; otherwise it spends one credit and hands the bytes to the
    /// writer thread over the bounded channel.
    pub fn push_chunk(&mut self, stream_id: u64, body: Vec<u8>) {
        let Some((conn_id, has_credit)) =
            self.streams.get(&stream_id).map(|stream| (stream.conn_id, stream.credit_outstanding > 0))
        else {
            // No such stream — already ended / torn down, or never opened.
            return;
        };
        if !has_credit {
            // Over-window flood (ADR-0128 §Consequences trust boundary).
            self.teardown_stream(stream_id);
            self.close_connection(conn_id, "stream credit exceeded");
            return;
        }
        let send_result = {
            let stream = self.streams.get_mut(&stream_id).expect("stream present under the same borrow");
            stream.credit_outstanding -= 1;
            stream.tx.try_send(WriterMsg::Chunk(body))
        };
        if send_result.is_err() {
            // Writer gone, or (defensively) the channel was full despite the
            // credit invariant — tear the stream down rather than block.
            self.teardown_stream(stream_id);
            self.close_connection(conn_id, "stream writer unavailable");
        }
    }

    /// The handler terminated the stream (ADR-0128): hand the terminator to
    /// the writer. If the bounded channel is full it is deferred and flushed
    /// as slots free ([`Self::try_flush_end`]), so the terminating chunk
    /// always follows the body in order.
    pub fn end_stream(&mut self, stream_id: u64) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.pending_end = true;
        }
        self.try_flush_end(stream_id);
    }

    /// A writer slot freed (ADR-0128): grant one more credit to the handler
    /// (unless the stream is ending, when no more chunks are expected) and
    /// try to flush a deferred terminator.
    pub fn replenish_credit(&mut self, ctx: &mut NativeCtx<'_>, stream_id: u64) {
        let grant = match self.streams.get_mut(&stream_id) {
            Some(stream) if !stream.pending_end => {
                stream.credit_outstanding += 1;
                true
            }
            Some(_) => false,
            None => return,
        };
        if grant {
            self.send_stream_credit(ctx, stream_id, 1);
        }
        self.try_flush_end(stream_id);
    }

    /// A writer thread finished (ADR-0128) — tear the stream down, then
    /// either resume the connection for the next request (keep-alive,
    /// mirroring the buffered path's decision) or close it. A keep-alive
    /// stream that outran the reader's response deadline finds the reader
    /// already gone; `resume_connection`'s send fails and it falls back to
    /// `close_connection` on that same path.
    pub fn finish_stream(&mut self, stream_id: u64) {
        let Some((conn_id, keep_alive)) =
            self.streams.get(&stream_id).map(|stream| (stream.conn_id, stream.keep_alive))
        else {
            return;
        };
        self.teardown_stream(stream_id);
        if keep_alive {
            self.resume_connection(conn_id);
        } else {
            self.close_connection(conn_id, "stream finished");
        }
    }

    /// Try to hand a deferred terminator to the writer; on success clear the
    /// pending flag so it is sent exactly once.
    fn try_flush_end(&mut self, stream_id: u64) {
        if let Some(stream) = self.streams.get_mut(&stream_id)
            && stream.pending_end
            && stream.tx.try_send(WriterMsg::End).is_ok()
        {
            stream.pending_end = false;
        }
    }

    /// Send the stream's handler a credit grant on `stream_id`. A fresh causal
    /// root per grant keeps credit mails settling per-chunk, never holding one
    /// chain open across the stream (ADR-0128 §4). The handler is the one
    /// resolved and stored at stream open (a response stream's matched route
    /// registrant, or a websocket's handshake handler, ADR-0129).
    pub fn send_stream_credit(&self, ctx: &mut NativeCtx<'_>, stream_id: u64, credit: u32) {
        let Some(handler) = self.streams.get(&stream_id).map(|stream| stream.handler) else {
            return;
        };
        let payload = HttpStreamCredit { stream_id, credit }.encode_into_bytes();
        let _ = ctx.send_envelope_detached(handler, <HttpStreamCredit as Kind>::ID, &payload);
    }

    /// Remove a stream and detach its writer thread without joining inline —
    /// the dispatcher must never block on a slow-peer write. Dropping the
    /// sender unblocks a recv-waiting writer; a socket shutdown by the caller
    /// unblocks a write-blocked one.
    pub fn teardown_stream(&mut self, stream_id: u64) {
        if let Some(mut stream) = self.streams.remove(&stream_id) {
            drop(stream.writer_thread.take());
        }
    }
}
