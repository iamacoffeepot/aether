//! `aether.tcp.session` — instanced actor, one per accepted
//! connection. Owns a `TcpStream` (split for read/write) and a
//! sidecar read thread that loops on blocking `read()`. The read
//! thread pushes byte chunks (or an EOF / error signal) over an
//! mpsc and fires a [`SessionDataReady`] mail at this actor's own
//! mailbox; the dispatcher drains them.
//!
//! Writes go directly from the dispatcher thread (`on_session_write`
//! does a blocking `write_all` on the write half). The read path
//! needs the sidecar because `read()` blocks indefinitely until
//! peer data or close; the write path doesn't need it because the
//! caller initiates writes synchronously and they're typically
//! fast.
//!
//! Shutdown: `unwire` flips the read thread's shutdown flag and
//! calls `stream.shutdown(Both)` on the write half. The kernel
//! aborts any blocked `read()` on the read half, the read thread
//! sees the error / EOF, exits, and the dispatcher joins it.
//!
//! Issue 775 retired the publish path: pre-#775 the dispatcher
//! re-broadcast every chunk as `SessionData` and the close as
//! `SessionClosed` through the `hub.claude.broadcast` mailbox.
//! With `BroadcastCapability` gone the chassis no longer fans
//! observation out, so this actor drops bytes on the floor today.
//! A future user-space TCP observer (monitor-based or session-
//! targeted mail) is the replacement path.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds need to be importable at file root for
// the `#[bridge]`-emitted `HandlesKind` markers.
use super::kinds::{SessionClose, SessionDataReady, SessionWrite};

#[aether_actor::bridge(instanced, one_per = "connection")]
mod session_native {
    use super::{SessionClose, SessionDataReady, SessionWrite};
    use aether_actor::actor;
    use aether_data::Kind;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::{KindId, Mail, Mailer};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::thread::JoinHandle;

    use crate::tcp::config::TcpSessionConfig;

    /// Default per-read buffer size. 64 KiB matches the typical
    /// kernel TCP buffer; any larger and we just block waiting for
    /// the kernel to fill it. Smaller adds syscall overhead per
    /// chunk.
    const READ_BUFFER_BYTES: usize = 64 * 1024;

    /// One end of a split `TcpStream`. The read sidecar owns the
    /// read half; the dispatcher owns the write half (used by
    /// `on_session_write`). Read-side errors / EOF flow back to the
    /// dispatcher via the `bytes_rx` channel as `Err(reason)`; the
    /// dispatcher discards them today (issue 775 retired the
    /// SessionData/SessionClosed broadcast path).
    pub struct TcpSessionActor {
        peer: String,
        session_name: String,
        write_half: TcpStream,
        shutdown: Arc<AtomicBool>,
        read_thread: Option<JoinHandle<()>>,
        bytes_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
    }

    #[actor]
    impl NativeActor for TcpSessionActor {
        type Config = TcpSessionConfig;
        const NAMESPACE: &'static str = "aether.tcp.session";

        fn init(
            mut config: TcpSessionConfig,
            ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            let stream = config
                .stream
                .take()
                .expect("TcpSessionConfig::stream consumed exactly once");
            // Split read/write via try_clone — both halves point at
            // the same underlying socket, but each is independently
            // owned. Read sidecar uses one for blocking reads; the
            // dispatcher uses the other for writes + Shutdown.
            let read_half = stream
                .try_clone()
                .map_err(|e| BootError::Other(Box::new(e)))?;
            let write_half = stream;

            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_for_thread = Arc::clone(&shutdown);

            // mpsc carrying read chunks OR an Err signaling EOF /
            // read error. The dispatcher drains in `on_data_ready`
            // and turns each item into a SessionData broadcast (Ok)
            // or a SessionClosed broadcast + ctx.shutdown() (Err).
            let (bytes_tx, bytes_rx) = mpsc::channel::<Result<Vec<u8>, String>>();

            let mailer_for_thread: Arc<Mailer> = ctx.mailer();
            let self_id = ctx.self_id();
            let data_ready_kind = KindId(<SessionDataReady as Kind>::ID.0);

            let thread_name = format!("aether-tcp-read-{}", config.session_name);
            // Transport thread below the mail layer — it carries inbound mail in;
            // no inbound chain to inherit, so no settlement umbrella to honor.
            #[allow(clippy::disallowed_methods)]
            let thread = thread::Builder::new()
                .name(thread_name)
                .spawn(move || {
                    let mut read_half = read_half;
                    let mut buf = vec![0u8; READ_BUFFER_BYTES];
                    loop {
                        if shutdown_for_thread.load(Ordering::Acquire) {
                            break;
                        }
                        match read_half.read(&mut buf) {
                            Ok(0) => {
                                let _ = bytes_tx.send(Err("eof".to_owned()));
                                mailer_for_thread.push(Mail::new(
                                    self_id,
                                    data_ready_kind,
                                    SessionDataReady::default().encode_into_bytes(),
                                    1,
                                ));
                                break;
                            }
                            Ok(n) => {
                                let chunk = buf[..n].to_vec();
                                if bytes_tx.send(Ok(chunk)).is_err() {
                                    break;
                                }
                                mailer_for_thread.push(Mail::new(
                                    self_id,
                                    data_ready_kind,
                                    SessionDataReady::default().encode_into_bytes(),
                                    1,
                                ));
                            }
                            Err(e) => {
                                if shutdown_for_thread.load(Ordering::Acquire) {
                                    break;
                                }
                                let reason = format!("read error: {e}");
                                let _ = bytes_tx.send(Err(reason));
                                mailer_for_thread.push(Mail::new(
                                    self_id,
                                    data_ready_kind,
                                    SessionDataReady::default().encode_into_bytes(),
                                    1,
                                ));
                                break;
                            }
                        }
                    }
                })
                .map_err(|e| BootError::Other(Box::new(e)))?;

            tracing::info!(
                target: "aether_substrate::tcp",
                session = %config.session_name,
                peer = %config.peer,
                "tcp session opened",
            );

            Ok(Self {
                peer: config.peer,
                session_name: config.session_name,
                write_half,
                shutdown,
                read_thread: Some(thread),
                bytes_rx,
            })
        }

        fn unwire(&mut self, _ctx: &mut NativeCtx<'_>) {
            self.shutdown.store(true, Ordering::Release);
            // Aborting the socket from the write half wakes any
            // blocked `read()` on the read half (same underlying fd).
            // Best-effort: a peer that already closed gives EBADF or
            // ENOTCONN here, which is fine.
            let _ = self.write_half.shutdown(Shutdown::Both);
            if let Some(t) = self.read_thread.take() {
                let _ = t.join();
            }
            tracing::info!(
                target: "aether_substrate::tcp",
                session = %self.session_name,
                peer = %self.peer,
                "tcp session closed",
            );
        }

        /// Sidecar read wake. Drain every pending chunk; `Ok` bytes
        /// are dropped (issue 775 retired the `SessionData` broadcast)
        /// and `Err` ends the session via `ctx.shutdown()`. One wake
        /// fires per chunk, but the handler drains until the queue
        /// is empty so coalesced wakes process all outstanding chunks
        /// in one dispatcher tick.
        #[handler]
        fn on_data_ready(&mut self, ctx: &mut NativeCtx<'_>, _mail: SessionDataReady) {
            while let Ok(item) = self.bytes_rx.try_recv() {
                match item {
                    Ok(_bytes) => {
                        // Bytes drop on the floor pending a user-space
                        // TCP observer rewire (issue 775).
                    }
                    Err(_reason) => {
                        ctx.shutdown();
                        return;
                    }
                }
            }
        }

        /// Write `bytes` to the connected peer. Blocking write on
        /// the dispatcher thread; for chunks larger than the kernel
        /// buffer this can block briefly, but typical request /
        /// response traffic clears in microseconds.
        #[handler]
        fn on_session_write(&mut self, ctx: &mut NativeCtx<'_>, mail: SessionWrite) {
            if let Err(e) = self.write_half.write_all(&mail.bytes) {
                tracing::warn!(
                    target: "aether_substrate::tcp",
                    session = %self.session_name,
                    peer = %self.peer,
                    error = %e,
                    "tcp session write failed",
                );
                ctx.shutdown();
            }
        }

        /// Cooperative external close. Peer mails this, we call
        /// `ctx.shutdown()`, the dispatcher drains remaining inbox
        /// mail, runs `unwire` (which joins the read thread).
        // Stateless close-request handler: `&mut self` is required by
        // the dispatch ABI (ADR-0033 / ADR-0038); shutdown is via ctx.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_close_request(&mut self, ctx: &mut NativeCtx<'_>, _mail: SessionClose) {
            ctx.shutdown();
        }
    }
}
