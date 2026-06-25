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
// the `#[actor]`-emitted `HandlesKind` markers against the identity
// (always-on, outside the `feature = "runtime"` gate).
use super::kinds::{SessionClose, SessionDataReady, SessionWrite};

/// `aether.tcp.session` **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the instanced
/// `OnePer("connection")` name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`TcpSessionState`, which holds the
/// `TcpStream` write half + the read thread) lives behind the one
/// `feature = "runtime"` gate, so a transport-only build never names
/// `TcpSessionState` nor pulls `aether_substrate` through this actor.
pub struct TcpSessionActor;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` / `std::net` type — the
// handler/init ctx, the runtime state, the read thread — lives in the
// `runtime` module below, gated once by `feature = "runtime"` and reached
// through the single `use runtime::*` glob.
use aether_actor::actor;

#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

#[cfg(feature = "runtime")]
mod runtime;

#[actor(instanced)]
impl NativeActor for TcpSessionActor {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// write-half + read-thread bundle.
    type State = TcpSessionState;
    type Config = TcpSessionConfig;
    const NAMESPACE: &'static str = "aether.tcp.session";

    fn init(
        mut config: TcpSessionConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<TcpSessionState, BootError> {
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

        Ok(TcpSessionState {
            peer: config.peer,
            session_name: config.session_name,
            write_half,
            shutdown,
            read_thread: Some(thread),
            bytes_rx,
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        state.shutdown.store(true, Ordering::Release);
        // Aborting the socket from the write half wakes any
        // blocked `read()` on the read half (same underlying fd).
        // Best-effort: a peer that already closed gives EBADF or
        // ENOTCONN here, which is fine.
        let _ = state.write_half.shutdown(Shutdown::Both);
        if let Some(t) = state.read_thread.take() {
            let _ = t.join();
        }
        tracing::info!(
            target: "aether_substrate::tcp",
            session = %state.session_name,
            peer = %state.peer,
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
    fn on_data_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: SessionDataReady) {
        while let Ok(item) = state.bytes_rx.try_recv() {
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
    fn on_session_write(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SessionWrite) {
        if let Err(e) = state.write_half.write_all(&mail.bytes) {
            tracing::warn!(
                target: "aether_substrate::tcp",
                session = %state.session_name,
                peer = %state.peer,
                error = %e,
                "tcp session write failed",
            );
            ctx.shutdown();
        }
    }

    /// Cooperative external close. Peer mails this, we call
    /// `ctx.shutdown()`, the dispatcher drains remaining inbox
    /// mail, runs `unwire` (which joins the read thread).
    // Stateless close-request handler: shutdown is via ctx, so `_state`
    // is unused.
    #[handler]
    fn on_close_request(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: SessionClose) {
        ctx.shutdown();
    }
}
