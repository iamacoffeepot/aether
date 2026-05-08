//! `aether.tcp.session` — instanced actor, one per accepted
//! connection. Owns a `TcpStream` (split for read/write) and a
//! sidecar read thread that loops on blocking `read()`. Mirrors
//! [`crate::tcp::listener::TcpListenerActor`]'s wake-mail shape:
//! the read thread pushes byte chunks (or an EOF / error signal)
//! over an mpsc and fires a [`SessionDataReady`] mail at this
//! actor's own mailbox; the dispatcher's handler drains and
//! broadcasts each chunk as [`SessionData`].
//!
//! Writes go directly from the dispatcher thread (`on_session_write`
//! does a blocking `write_all` on the write half). The read path
//! needs the sidecar because `read()` blocks indefinitely until
//! peer data or close; the write path doesn't need it because the
//! caller initiates writes synchronously and they're typically
//! fast.
//!
//! Shutdown: `on_close` flips the read thread's shutdown flag and
//! calls `stream.shutdown(Both)` on the write half. The kernel
//! aborts any blocked `read()` on the read half, the read thread
//! sees the error / EOF, exits. The dispatcher joins the thread
//! and emits a single `SessionClosed` broadcast (suppressed if the
//! read path already broadcast one for an EOF / error).

// Handler-signature kinds need to be importable at file root for
// the `#[bridge]`-emitted `HandlesKind` markers.
use aether_kinds::{SessionClose, SessionDataReady, SessionWrite};

// `TcpSessionActor` is auto-re-exported by `#[bridge]` at file
// root; only `TcpSessionConfig` needs the manual re-export.
#[cfg(not(target_arch = "wasm32"))]
pub use session_native::TcpSessionConfig;

#[aether_actor::bridge(instanced)]
mod session_native {
    use super::{SessionClose, SessionDataReady, SessionWrite};
    use aether_actor::{Sender, actor};
    use aether_data::Kind;
    use aether_kinds::{HUB_BROADCAST_MAILBOX_NAME, SessionClosed, SessionData};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::{KindId, Mail, Mailer};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    /// Default per-read buffer size. 64 KiB matches the typical
    /// kernel TCP buffer; any larger and we just block waiting for
    /// the kernel to fill it. Smaller adds syscall overhead per
    /// chunk. Not currently configurable; agents that want
    /// different framing can broadcast on top of `SessionData` and
    /// re-chunk in user-space.
    const READ_BUFFER_BYTES: usize = 64 * 1024;

    /// Init config for [`TcpSessionActor`]. The listener's
    /// `on_connection_ready` builds this per accepted stream and
    /// hands it through `spawn_child`. `stream` is `Option` so init
    /// can `.take()` and split it; `peer` and `session_name` are
    /// echoed in every broadcast for agent-side correlation.
    pub struct TcpSessionConfig {
        pub stream: Option<TcpStream>,
        pub peer: String,
        pub session_name: String,
    }

    /// One end of a split `TcpStream`. The read sidecar owns the
    /// read half; the dispatcher owns the write half (used by
    /// `on_session_write`). Read-side errors / EOF flow back to the
    /// dispatcher via the `bytes_rx` channel as `Err(reason)`.
    pub struct TcpSessionActor {
        peer: String,
        session_name: String,
        write_half: TcpStream,
        shutdown: Arc<AtomicBool>,
        read_thread: Option<JoinHandle<()>>,
        bytes_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
        mailer: Arc<Mailer>,
        // Sticks to true once a `SessionClosed` broadcast has fired
        // so duplicate broadcasts don't pile up if both the read
        // path and `on_close` see the close.
        closed_emitted: bool,
    }

    #[actor]
    impl NativeActor for TcpSessionActor {
        type Config = TcpSessionConfig;
        const NAMESPACE: &'static str = "aether.tcp.session";
        const SCHEDULING: Scheduling = Scheduling::Dedicated;

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

            let mailer: Arc<Mailer> = ctx.mailer();
            let mailer_for_thread = Arc::clone(&mailer);
            let self_id = ctx.self_id();
            let data_ready_kind = KindId(<SessionDataReady as Kind>::ID.0);

            let thread_name = format!("aether-tcp-read-{}", config.session_name);
            let thread = std::thread::Builder::new()
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
                                    Vec::new(),
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
                                    Vec::new(),
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
                                    Vec::new(),
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
                mailer,
                closed_emitted: false,
            })
        }

        fn on_close(&mut self, _ctx: &mut NativeCtx<'_>) {
            self.shutdown.store(true, Ordering::Release);
            // Aborting the socket from the write half wakes any
            // blocked `read()` on the read half (same underlying fd).
            // Best-effort: a peer that already closed gives EBADF or
            // ENOTCONN here, which is fine.
            let _ = self.write_half.shutdown(Shutdown::Both);
            if let Some(t) = self.read_thread.take() {
                let _ = t.join();
            }
            // Emit one SessionClosed if the read path didn't already
            // (e.g. an explicit close before EOF was observed).
            if !self.closed_emitted {
                self.closed_emitted = true;
                broadcast_via_mailer(
                    &self.mailer,
                    &SessionClosed {
                        session_name: self.session_name.clone(),
                        peer: self.peer.clone(),
                        reason: "explicit close".to_owned(),
                    },
                );
            }
            tracing::info!(
                target: "aether_substrate::tcp",
                session = %self.session_name,
                peer = %self.peer,
                "tcp session closed",
            );
        }

        /// Sidecar read wake. Drain every pending chunk: each `Ok`
        /// becomes a [`SessionData`] broadcast; an `Err` ends the
        /// session (broadcast `SessionClosed`, call `ctx.shutdown()`).
        ///
        /// One wake fires per chunk, but the handler drains until
        /// the queue is empty so coalesced wakes process all
        /// outstanding chunks in one dispatcher tick.
        #[handler]
        fn on_data_ready(&mut self, ctx: &mut NativeCtx<'_>, _mail: SessionDataReady) {
            while let Ok(item) = self.bytes_rx.try_recv() {
                match item {
                    Ok(bytes) => {
                        let payload = SessionData {
                            session_name: self.session_name.clone(),
                            peer: self.peer.clone(),
                            bytes,
                        };
                        broadcast(ctx, &payload);
                    }
                    Err(reason) => {
                        if !self.closed_emitted {
                            self.closed_emitted = true;
                            let payload = SessionClosed {
                                session_name: self.session_name.clone(),
                                peer: self.peer.clone(),
                                reason,
                            };
                            broadcast(ctx, &payload);
                        }
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
                if !self.closed_emitted {
                    self.closed_emitted = true;
                    let payload = SessionClosed {
                        session_name: self.session_name.clone(),
                        peer: self.peer.clone(),
                        reason: format!("write error: {e}"),
                    };
                    broadcast(ctx, &payload);
                }
                ctx.shutdown();
            }
        }

        /// Cooperative external close. Same shape as the listener
        /// pattern: peer mails this, we call `ctx.shutdown()`, the
        /// dispatcher drains remaining inbox mail, runs `on_close`
        /// (which joins the read thread and emits `SessionClosed`).
        #[handler]
        fn on_close_request(&mut self, ctx: &mut NativeCtx<'_>, _mail: SessionClose) {
            ctx.shutdown();
        }
    }

    /// Best-effort hub broadcast from inside a handler — uses the
    /// ctx's `Sender::send_to_named` path for typed addressing.
    fn broadcast<K: Kind + serde::Serialize>(ctx: &mut NativeCtx<'_>, payload: &K) {
        ctx.send_to_named::<K>(HUB_BROADCAST_MAILBOX_NAME, payload);
    }

    /// Broadcast variant for `on_close` where we want to use the
    /// stored `mailer` reference directly (still inside the
    /// dispatcher thread). Functionally equivalent — the
    /// `send_to_named` path also goes through `Mailer::push` —
    /// kept separate to avoid borrowing `&mut self` and `ctx`
    /// simultaneously when computing the payload from `self` fields.
    fn broadcast_via_mailer<K: Kind + serde::Serialize>(mailer: &Arc<Mailer>, payload: &K) {
        let bytes = match postcard::to_allocvec(payload) {
            Ok(b) => b,
            Err(_) => return,
        };
        let kind = KindId(K::ID.0);
        let mailbox = aether_data::mailbox_id_from_name(HUB_BROADCAST_MAILBOX_NAME);
        mailer.push(Mail::new(
            aether_substrate::MailboxId(mailbox.0),
            kind,
            bytes,
            1,
        ));
    }
}
