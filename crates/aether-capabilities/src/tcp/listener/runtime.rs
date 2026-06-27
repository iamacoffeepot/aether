//! The `aether.tcp.listener` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpListenerActor`](super::TcpListenerActor) identity never names these
//! types nor pulls `aether_substrate`. The substrate / `std::net`-typed
//! imports are gated once by this module rather than line-by-line; the
//! `#[actor] impl` reaches the state, ctx types, and config / session types
//! through the single `use runtime::*` glob in the parent.

pub(super) use std::net::{SocketAddr, TcpStream};
pub(super) use std::sync::Arc;
pub(super) use std::sync::atomic::{AtomicBool, Ordering};
pub(super) use std::sync::mpsc;
pub(super) use std::thread::{self, JoinHandle};
pub(super) use std::time::Duration;

pub(super) use aether_data::Kind;
pub(super) use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub(super) use aether_substrate::chassis::error::BootError;
pub(super) use aether_substrate::{KindId, Mail, Mailer};

pub(super) use crate::tcp::config::{TcpListenerConfig, TcpSessionConfig};
pub(super) use crate::tcp::session::TcpSessionActor;

use aether_actor::runtime;
// The moved handler bodies name the cap kinds backing their signatures; bring
// them in crate-absolute, matching the style above.
use crate::tcp::kinds::{Close, ConnectionReady};
// The `#[runtime] impl NativeActor` names the identity struct from the parent.
use super::TcpListenerActor;

/// `aether.tcp.listener` runtime state (issue 607 Phase 6b, ADR-0079). The
/// accept thread can't call `ctx.spawn_child` (no dispatcher ctx), so it
/// pushes accepted streams over `connection_rx` and fires a
/// [`ConnectionReady`](super::ConnectionReady) wake mail. The dispatcher's
/// `on_connection_ready` handler drains the mpsc and spawns one
/// `TcpSessionActor` per pending stream. The addressing identity is the
/// distinct ZST [`TcpListenerActor`](super::TcpListenerActor).
pub struct TcpListenerState {
    pub(super) local_port: u16,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) accept_thread: Option<JoinHandle<()>>,
    connection_rx: mpsc::Receiver<(TcpStream, SocketAddr)>,
    next_subname: u64,
}

#[runtime]
impl NativeActor for TcpListenerActor {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// accept-thread + connection-channel bundle.
    type State = TcpListenerState;
    type Config = TcpListenerConfig;
    const NAMESPACE: &'static str = "aether.tcp.listener";

    fn init(
        mut config: TcpListenerConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<TcpListenerState, BootError> {
        let listener = config
            .listener
            .take()
            .expect("TcpListenerConfig::listener consumed exactly once");
        let addr = config.addr;
        let port = config.port;
        // Stay blocking — the accept loop wakes via self-connect
        // on `unwire`. Nonblocking would require a poll loop +
        // CPU burn for no win.
        listener
            .set_nonblocking(false)
            .map_err(|e| BootError::Other(Box::new(e)))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);

        // mpsc for accept→dispatcher stream handoff. Unbounded —
        // the kernel's accept backlog already bounds incoming
        // connections, and the dispatcher drains the channel on
        // every `ConnectionReady` mail.
        let (connection_tx, connection_rx) = mpsc::channel::<(TcpStream, SocketAddr)>();

        // Wake-mail plumbing: capture the mailer + this actor's
        // own MailboxId so the accept thread can fire a
        // ConnectionReady mail at us per accept.
        let mailer: Arc<Mailer> = ctx.mailer();
        let self_id = ctx.self_id();
        let connection_ready_kind = KindId(<ConnectionReady as Kind>::ID.0);

        // Transport thread below the mail layer — it carries inbound mail in;
        // no inbound chain to inherit, so no settlement umbrella to honor.
        #[allow(clippy::disallowed_methods)]
        let thread = thread::Builder::new()
            .name(format!("aether-tcp-accept-{port}"))
            .spawn(move || {
                while !shutdown_for_thread.load(Ordering::Acquire) {
                    if let Ok((stream, peer)) = listener.accept() {
                        if shutdown_for_thread.load(Ordering::Acquire) {
                            drop(stream);
                            break;
                        }
                        if connection_tx.send((stream, peer)).is_err() {
                            // Dispatcher's receiver gone — actor
                            // is shutting down or already dropped.
                            break;
                        }
                        // Wake the dispatcher: the actual
                        // stream is in the mpsc; this mail just
                        // signals "drain me". The payload is the
                        // wake kind's own wire image (an empty image
                        // for a fieldless wire kind, ADR-0118) so the
                        // typed handler decodes it.
                        mailer.push(Mail::new(
                            self_id,
                            connection_ready_kind,
                            ConnectionReady::default().encode_into_bytes(),
                            1,
                        ));
                    } else if shutdown_for_thread.load(Ordering::Acquire) {
                        break;
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        tracing::info!(
            target: "aether_substrate::tcp",
            addr = %addr,
            port = port,
            "tcp listener bound",
        );

        Ok(TcpListenerState {
            local_port: port,
            shutdown,
            accept_thread: Some(thread),
            connection_rx,
            next_subname: 0,
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        state.shutdown.store(true, Ordering::Release);
        // Wake the blocked accept(). Self-connect to the bound
        // port; the accept returns, sees the flag, breaks. Short
        // connect timeout so a misconfigured listener (port
        // unreachable) doesn't hang the close path.
        let addr_str = format!("127.0.0.1:{}", state.local_port);
        if let Ok(addr) = addr_str.parse::<SocketAddr>() {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
        }
        if let Some(thread) = state.accept_thread.take() {
            let _ = thread.join();
        }
        tracing::info!(
            target: "aether_substrate::tcp",
            port = state.local_port,
            "tcp listener closed",
        );
    }

    /// Cooperative external close. The unbind path on
    /// `TcpCapability` mails this; we shut down so the dispatcher
    /// drains, runs `unwire`, and the close fan-out fires
    /// `MonitorNotice` to the cap.
    // Stateless close request: shutdown is requested through `ctx`, not
    // through any state field, so `_state` is unused.
    #[handler]
    fn on_close_request(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: Close) {
        ctx.shutdown();
    }

    /// Sidecar wake. Drain every pending accepted connection and
    /// spawn a `TcpSessionActor` per stream. Each session is a
    /// child of this listener (parent `Source` stamps as our own
    /// mailbox), so on session close the close fan-out reaches
    /// us via the standard monitor path.
    ///
    /// The accept thread fires one wake mail per accepted
    /// connection, but the handler drains until empty regardless
    /// — if multiple wakes coalesce into one dispatcher tick,
    /// we'll see the queue already drained on the second handler
    /// call and exit fast.
    #[handler]
    fn on_connection_ready(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _mail: ConnectionReady,
    ) {
        while let Ok((stream, peer)) = state.connection_rx.try_recv() {
            let subname = format!("conn-{}", state.next_subname);
            state.next_subname += 1;
            let peer_str = peer.to_string();
            let session_config = TcpSessionConfig {
                stream: Some(stream),
                peer: peer_str.clone(),
                session_name: subname.clone(),
            };
            match ctx
                .spawn_child::<TcpSessionActor>(
                    aether_substrate::Subname::Named(&subname),
                    session_config,
                )
                .finish()
            {
                Ok(_) => {
                    tracing::debug!(
                        target: "aether_substrate::tcp",
                        session = %subname,
                        peer = %peer_str,
                        "tcp session spawned",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::tcp",
                        session = %subname,
                        peer = %peer_str,
                        error = ?e,
                        "tcp session spawn failed; closing stream",
                    );
                }
            }
        }
    }
}
