//! The `aether.tcp.listener` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpListenerActor`](super::TcpListenerActor) identity never names these
//! types nor pulls `aether_substrate`. The substrate / `std::net`-typed
//! imports are gated once by this module rather than line-by-line; the
//! `#[actor] impl` reaches the state, ctx types, and config / session types
//! through the single `use runtime::*` glob in the parent.

pub use std::net::{SocketAddr, TcpListener, TcpStream};
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::sync::mpsc;
pub use std::thread::{self, JoinHandle};
pub use std::time::Duration;

pub use aether_data::Kind;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::{KindId, Mail, Mailer};

pub use crate::config::{TcpListenerConfig, TcpSessionConfig};
pub use crate::session::TcpSessionActor;

use aether_actor::runtime;
// The moved handler bodies name the cap kinds backing their signatures; bring
// them in crate-absolute, matching the style above.
use crate::kinds::{Close, ConnectionReady};
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
    pub local_port: u16,
    pub consumer: Option<aether_data::MailboxId>,
    pub shutdown: Arc<AtomicBool>,
    pub accept_start: Option<mpsc::Sender<()>>,
    pub accept_thread: Option<JoinHandle<()>>,
    pub connection_rx: mpsc::Receiver<(TcpStream, SocketAddr)>,
    pub next_subname: u64,
}

/// Completion context for a staged accepted-connection birth. The child's
/// identity rides its `SpawnOutcome`; what this carries is the peer address the
/// accept loop observed, which the spawn itself never learns.
#[derive(Clone)]
pub struct AcceptedSessionContext {
    pub session_name: String,
    pub peer: String,
}

impl TcpListenerState {
    fn stop_accept_thread(&mut self) {
        let Some(thread) = self.accept_thread.take() else {
            self.accept_start.take();
            return;
        };
        self.shutdown.store(true, Ordering::Release);
        let was_parked = self.accept_start.take().is_some();
        if !was_parked {
            let addr_str = format!("127.0.0.1:{}", self.local_port);
            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
            }
        }
        let _ = thread.join();
    }
}

impl Drop for TcpListenerState {
    fn drop(&mut self) {
        self.stop_accept_thread();
    }
}

fn run_accept_loop(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    connection_tx: mpsc::Sender<(TcpStream, SocketAddr)>,
    accept_start_rx: mpsc::Receiver<()>,
    mailer: Arc<Mailer>,
    self_id: aether_data::MailboxId,
    connection_ready_kind: KindId,
) {
    if accept_start_rx.recv().is_err() {
        return;
    }
    while !shutdown.load(Ordering::Acquire) {
        if let Ok((stream, peer)) = listener.accept() {
            if shutdown.load(Ordering::Acquire) {
                drop(stream);
                break;
            }
            if connection_tx.send((stream, peer)).is_err() {
                break;
            }
            // The stream stays in the actor-owned channel; this mail is only
            // the typed wake that makes the dispatcher drain it.
            mailer.push(Mail::new(self_id, connection_ready_kind, ConnectionReady::default().encode_into_bytes(), 1));
        } else if shutdown.load(Ordering::Acquire) {
            break;
        }
    }
}

#[runtime]
impl NativeActor for TcpListenerActor {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// accept-thread + connection-channel bundle.
    type State = TcpListenerState;
    type Config = TcpListenerConfig;
    const NAMESPACE: &'static str = "aether.tcp.listener";

    fn init(mut config: TcpListenerConfig, ctx: &mut NativeInitCtx<'_>) -> Result<TcpListenerState, BootError> {
        let listener = config.listener.take().expect("TcpListenerConfig::listener consumed exactly once");
        let addr = config.addr;
        let port = config.port;
        // Stay blocking — the accept loop wakes via self-connect
        // on `unwire`. Nonblocking would require a poll loop +
        // CPU burn for no win.
        listener.set_nonblocking(false).map_err(|e| BootError::Other(Box::new(e)))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);

        // mpsc for accept→dispatcher stream handoff. Unbounded —
        // the kernel's accept backlog already bounds incoming
        // connections, and the dispatcher drains the channel on
        // every `ConnectionReady` mail.
        let (connection_tx, connection_rx) = mpsc::channel::<(TcpStream, SocketAddr)>();
        // The route does not become dispatchable until owner-time activation
        // has run `wire`. Keep accept parked until then so an early connection
        // cannot enqueue a wake against a Starting actor.
        let (accept_start_tx, accept_start_rx) = mpsc::channel::<()>();

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
                run_accept_loop(
                    listener,
                    shutdown_for_thread,
                    connection_tx,
                    accept_start_rx,
                    mailer,
                    self_id,
                    connection_ready_kind,
                );
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        tracing::info!(
            target: "aether_tcp",
            addr = %addr,
            port = port,
            "tcp listener bound",
        );

        Ok(TcpListenerState {
            local_port: port,
            consumer: config.consumer,
            shutdown,
            accept_start: Some(accept_start_tx),
            accept_thread: Some(thread),
            connection_rx,
            next_subname: 0,
        })
    }

    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if let Some(start) = state.accept_start.take() {
            let _ = start.send(());
        }
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        // Pre-wire rollback reaches the same helper through `Drop`. A live
        // listener self-connects to wake `accept`; a parked one cancels the
        // gate, and both paths join before the state is released.
        state.stop_accept_thread();
        tracing::info!(
            target: "aether_tcp",
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
    #[handler::single]
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
    #[handler::single]
    fn on_connection_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: ConnectionReady) {
        while let Ok((stream, peer)) = state.connection_rx.try_recv() {
            let subname = format!("conn-{}", state.next_subname);
            state.next_subname += 1;
            let peer_str = peer.to_string();
            let session_config = TcpSessionConfig {
                stream: Some(stream),
                peer: peer_str.clone(),
                session_name: subname.clone(),
                consumer: state.consumer,
            };
            match ctx
                .spawn_child::<TcpListenerActor, TcpSessionActor>(
                    aether_substrate::Subname::Named(&subname),
                    session_config,
                    (),
                )
                .stage_with(AcceptedSessionContext { session_name: subname.clone(), peer: peer_str.clone() })
            {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "aether_tcp",
                        session = %subname,
                        peer = %peer_str,
                        error = ?e,
                        "tcp session spawn failed; closing stream",
                    );
                }
            }
        }
    }

    #[handler(task)]
    fn on_session_spawn_done(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome, AcceptedSessionContext>,
    ) {
        match &done.output().result {
            Ok(()) => {
                tracing::debug!(
                    target: "aether_tcp",
                    session = %done.context().session_name,
                    peer = %done.context().peer,
                    "tcp session spawned",
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_tcp",
                    session = %done.context().session_name,
                    peer = %done.context().peer,
                    error = ?error,
                    "tcp session spawn failed; stream closed during rollback",
                );
            }
        }
        done.release_no_reply();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_substrate::mail::registry::Registry;

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn early_connection_waits_behind_the_activation_gate() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind gated listener");
        let addr = listener.local_addr().expect("gated listener address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_loop = Arc::clone(&shutdown);
        let (connection_tx, connection_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let thread = thread::spawn(move || {
            run_accept_loop(
                listener,
                shutdown_for_loop,
                connection_tx,
                start_rx,
                mailer,
                aether_data::MailboxId(0x4066),
                KindId(<ConnectionReady as Kind>::ID.0),
            );
        });

        let early_client = TcpStream::connect(addr).expect("kernel queues an early connection");
        assert!(
            matches!(connection_rx.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)),
            "the accept sidecar cannot consume before wire releases its gate",
        );

        start_tx.send(()).expect("release the production accept gate");
        let (_accepted, peer) =
            connection_rx.recv_timeout(Duration::from_secs(2)).expect("early connection is accepted");
        assert_eq!(peer, early_client.local_addr().expect("early client address"));

        shutdown.store(true, Ordering::Release);
        drop(early_client);
        let _wake = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
        thread.join().expect("accept loop exits after shutdown");
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn cancelling_the_activation_gate_does_not_strand_the_accept_thread() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind cancelled listener");
        let shutdown = Arc::new(AtomicBool::new(false));
        let (connection_tx, _connection_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let thread = thread::spawn(move || {
            run_accept_loop(
                listener,
                shutdown,
                connection_tx,
                start_rx,
                mailer,
                aether_data::MailboxId(0x4066_0001),
                KindId(<ConnectionReady as Kind>::ID.0),
            );
        });

        drop(start_tx);
        thread.join().expect("dropping the pre-wire gate joins without a socket wake");
    }
}
