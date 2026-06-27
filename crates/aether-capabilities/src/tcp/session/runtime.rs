//! The `aether.tcp.session` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpSessionActor`](super::TcpSessionActor) identity never names these
//! types nor pulls `aether_substrate`. The substrate / `std::net`-typed
//! imports are gated once by this module rather than line-by-line; the
//! `#[actor] impl` reaches the state, ctx types, and config through the
//! single `use runtime::*` glob in the parent.

// The moved `#[handler]` methods take their decoded payload by value per the
// dispatch contract; the by-value `SessionWrite` arg trips this lint.
#![allow(clippy::needless_pass_by_value)]

pub(super) use std::io::{Read, Write};
pub(super) use std::net::{Shutdown, TcpStream};
pub(super) use std::sync::Arc;
pub(super) use std::sync::atomic::{AtomicBool, Ordering};
pub(super) use std::sync::mpsc;
pub(super) use std::thread::{self, JoinHandle};

pub(super) use aether_data::Kind;
pub(super) use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub(super) use aether_substrate::chassis::error::BootError;
pub(super) use aether_substrate::{KindId, Mail, Mailer};

pub(super) use crate::tcp::config::TcpSessionConfig;

use aether_actor::runtime;
// The moved handler bodies name the cap kinds backing their signatures; bring
// them in crate-absolute, matching the style above.
use crate::tcp::kinds::{SessionClose, SessionDataReady, SessionWrite};
// The `#[runtime] impl NativeActor` names the identity struct from the parent.
use super::TcpSessionActor;

/// Default per-read buffer size. 64 KiB matches the typical
/// kernel TCP buffer; any larger and we just block waiting for
/// the kernel to fill it. Smaller adds syscall overhead per
/// chunk.
pub const READ_BUFFER_BYTES: usize = 64 * 1024;

/// `aether.tcp.session` runtime state (issue 607 Phase 6b, ADR-0079). One end
/// of a split `TcpStream`: the read sidecar owns the read half; the dispatcher
/// owns `write_half` (used by `on_session_write`). Read-side errors / EOF flow
/// back to the dispatcher via the `bytes_rx` channel as `Err(reason)`; the
/// dispatcher discards them today (issue 775 retired the
/// `SessionData` / `SessionClosed` broadcast path). The addressing identity is
/// the distinct ZST [`TcpSessionActor`](super::TcpSessionActor).
pub struct TcpSessionState {
    pub(super) peer: String,
    pub(super) session_name: String,
    pub(super) write_half: TcpStream,
    pub(super) shutdown: Arc<AtomicBool>,
    read_thread: Option<JoinHandle<()>>,
    bytes_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
}

#[runtime]
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
