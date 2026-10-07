//! The `aether.tcp.listener` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpListenerActor`] identity never names these
//! types nor pulls `aether_substrate`. The substrate / `std::net`-typed
//! imports are gated once by this module rather than line-by-line; the
//! `#[actor] impl` reaches the state, ctx types, and config / session types
//! through the single `use runtime::*` glob in the parent.

pub use std::collections::HashMap;
pub use std::net::{SocketAddr, TcpListener, TcpStream};
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::sync::mpsc;
pub use std::thread::JoinHandle;
pub use std::time::Duration;

pub use aether_substrate::actor::monitor::MonitorHandle;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone};
pub use aether_substrate::chassis::error::BootError;

pub use crate::config::{TcpListenerConfig, TcpSessionConfig};
pub use crate::session::TcpSessionActor;

use aether_actor::{ActorRef, Anyone, ErasedActorRef, ProtocolRef, Single, runtime};
// `MonitorNotice` is named by `on_monitor_notice`'s signature.
use aether_kinds::MonitorNotice;
// The moved handler bodies name the cap kinds backing their signatures; bring
// them in crate-absolute, matching the style above.
use crate::kinds::{Close, ConnectionReady, SessionClose, TcpConsumer};
// The `#[runtime] impl NativeActor` names the identity struct from the parent.
use super::TcpListenerActor;

/// `aether.tcp.listener` runtime state (issue 607 Phase 6b, ADR-0079). The
/// accept thread can't call `ctx.spawn_child` (no dispatcher ctx), so it
/// pushes accepted streams over `connection_rx` and fires a
/// [`ConnectionReady`] wake mail. The dispatcher's
/// `on_connection_ready` handler drains the mpsc and spawns one
/// `TcpSessionActor` per pending stream. The addressing identity is the
/// distinct ZST [`TcpListenerActor`].
pub struct TcpListenerState {
    pub local_port: u16,
    /// The consumer the cap proved at `BindListener` or `BindListenerSelf`
    /// receipt (ADR-0230, ADR-0231 §3/§4), handed to every session this
    /// listener accepts. This listener does not watch it: the cap monitors
    /// the consumer and mails `Close` here when it closes.
    pub consumer: ProtocolRef<TcpConsumer>,
    /// The live sessions this listener accepted, keyed by each session's
    /// reference, which is the sender of its close notice. The engine does
    /// not close an actor's children with it, so `unwire` mails each of
    /// these `SessionClose`.
    pub sessions: HashMap<ErasedActorRef, AcceptedSession>,
    /// Whether this listener still accepts, and what a `Close` waits for.
    pub accepting: Accepting,
    pub shutdown: Arc<AtomicBool>,
    pub accept_start: Option<mpsc::Sender<()>>,
    pub accept_thread: Option<JoinHandle<()>>,
    pub connection_rx: mpsc::Receiver<(TcpStream, SocketAddr)>,
    pub next_subname: u64,
}

/// One live session this listener accepted. Drops with the entry;
/// `MonitorHandle::Drop` is idempotent with the close path's index drain.
pub struct AcceptedSession {
    /// The reference the session's spawn outcome proved; `unwire` mails
    /// `SessionClose` through it. Its erased form keys this entry.
    pub session: ActorRef<TcpSessionActor>,
    // Held to keep this listener's monitor on the session registered until
    // the entry is removed (in `on_monitor_notice`).
    _monitor_handle: MonitorHandle,
}

/// Where a listener stands between accepting and closed. A session birth is
/// staged in one handler and settles in a later one, and only a settled
/// birth has an entry in `sessions` for `unwire` to close. So a `Close` that
/// arrives while births are unsettled waits for them rather than shutting
/// down past a session nothing would then close.
pub enum Accepting {
    /// Accepting connections, with `unsettled` session births staged and not
    /// yet completed.
    Open { unsettled: usize },
    /// `Close` arrived with `unsettled` births outstanding: new connections
    /// are dropped, and the listener shuts down when the last one settles.
    Closing { unsettled: usize },
}

impl Accepting {
    /// The staged session births whose completion has not arrived.
    fn unsettled(&mut self) -> &mut usize {
        match self {
            Self::Open { unsettled } | Self::Closing { unsettled } => unsettled,
        }
    }
}

/// Completion context for a staged accepted-connection birth, taken from the
/// ctx in its task completion (ADR-0243 §9). The child's identity rides its
/// `SpawnOutcome`; what this carries is the peer address the accept loop
/// observed, which the spawn itself never learns.
#[aether_data::kind(name = "aether.tcp.listener.accepted_session")]
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

/// Accept connections until `shutdown`, handing each stream over
/// `connection_tx` and calling `on_accept` to wake the dispatcher.
fn run_accept_loop(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    connection_tx: mpsc::Sender<(TcpStream, SocketAddr)>,
    accept_start_rx: mpsc::Receiver<()>,
    on_accept: impl Fn(),
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
            // The stream stays in the actor-owned channel; this is only the
            // typed wake that makes the dispatcher drain it.
            on_accept();
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

    fn init(config: TcpListenerConfig, ctx: &mut NativeInitCtx<'_>) -> Result<TcpListenerState, BootError> {
        let listener = config.listener;
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

        // The accept thread wakes this actor with one ConnectionReady per
        // accept, through a self-wake that names no position.
        let wake = ctx.self_wake::<ConnectionReady>();

        // Transport thread below the mail layer — it carries inbound mail in;
        // no inbound chain to inherit, so no settlement umbrella to honor.
        let thread = wake
            .clone()
            .spawn_sidecar(format!("aether-tcp-accept-{port}"), move || {
                run_accept_loop(listener, shutdown_for_thread, connection_tx, accept_start_rx, move || {
                    wake.wake(&ConnectionReady::default());
                });
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
            sessions: HashMap::new(),
            accepting: Accepting::Open { unsettled: 0 },
            shutdown,
            accept_start: Some(accept_start_tx),
            accept_thread: Some(thread),
            connection_rx,
            next_subname: 0,
        })
    }

    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        if let Some(start) = state.accept_start.take() {
            let _ = start.send(());
        }
        Ok(())
    }

    fn unwire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        // The engine does not close a closing actor's children, so each
        // accepted session is told to close here. An engine teardown reaches
        // this too, where the sessions are closing anyway.
        for accepted in state.sessions.values() {
            ctx.send_to(accepted.session, &SessionClose::default());
        }

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

    /// Cooperative external close. The cap mails this for an unbind and
    /// when this listener's consumer closes; we shut down so the dispatcher
    /// drains, runs `unwire`, and the close fan-out fires `MonitorNotice` to
    /// the cap.
    ///
    /// With session births unsettled the shutdown waits for them
    /// ([`Accepting`]): `on_session_spawn_done` requests it when the last
    /// one settles.
    #[handler::tell]
    fn on_close_request(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: Close) {
        let unsettled = *state.accepting.unsettled();
        state.accepting = Accepting::Closing { unsettled };
        if unsettled == 0 {
            ctx.shutdown();
        }
    }

    /// An accepted session tombstoned: drop its entry, and with it this
    /// listener's monitor on it.
    ///
    /// The host stamps the closed session as the notice's sender, so its
    /// entry is the one keyed by `ctx.sender()`. Sessions are the only
    /// actors this listener monitors, and a notice with no entry under its
    /// sender, or with no sender, changes nothing.
    #[handler::event]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        if let Some(departed) = ctx.sender() {
            state.sessions.remove(&departed);
        }
    }

    /// Sidecar wake. Drain every pending accepted connection and
    /// spawn a `TcpSessionActor` per stream. Each session is a
    /// child of this listener, entered in `sessions` when its birth
    /// settles. A listener that is closing drops the streams instead,
    /// which closes each connection.
    ///
    /// The accept thread fires one wake mail per accepted
    /// connection, but the handler drains until empty regardless
    /// — if multiple wakes coalesce into one dispatcher tick,
    /// we'll see the queue already drained on the second handler
    /// call and exit fast.
    #[handler::tell]
    fn on_connection_ready(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, Anyone, Single>,
        _mail: ConnectionReady,
    ) {
        while let Ok((stream, peer)) = state.connection_rx.try_recv() {
            let Accepting::Open { unsettled } = &mut state.accepting else {
                continue;
            };

            let subname = format!("conn-{}", state.next_subname);
            state.next_subname += 1;
            let peer_str = peer.to_string();
            let session_config = TcpSessionConfig {
                stream,
                peer: peer_str.clone(),
                session_name: subname.clone(),
                consumer: state.consumer,
            };
            match ctx
                .spawn_child::<TcpSessionActor>(aether_substrate::Subname::Named(&subname), session_config, ())
                .stage_with(AcceptedSessionContext { session_name: subname.clone(), peer: peer_str.clone() })
            {
                Ok(_) => *unsettled += 1,
                Err((e, _)) => {
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

    /// Settle one staged session birth: enter the activated session in
    /// `sessions` under this listener's monitor on it. A session that closed
    /// before this ran is entered all the same, and its notice, which arrives
    /// after this handler returns, removes the entry. A `Close` that was
    /// waiting on this birth shuts the listener down here, and `unwire`
    /// closes the session just entered.
    #[handler(task)]
    fn on_session_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<TcpSessionActor>>,
    ) {
        let born = done.into_output().result;
        if let Ok(session) = &born {
            let session = *session;
            state.sessions.insert(session.erase(), AcceptedSession { session, _monitor_handle: ctx.monitor(session) });
        }

        let unsettled = state.accepting.unsettled();
        *unsettled -= 1;
        let settled = *unsettled == 0;
        let closing = matches!(state.accepting, Accepting::Closing { .. });
        let closes = closing && settled;
        if closes {
            ctx.shutdown();
        }

        let Some(AcceptedSessionContext { session_name, peer }) = ctx.take_context() else {
            return;
        };
        match born {
            Ok(_) => {
                tracing::debug!(
                    target: "aether_tcp",
                    session = %session_name,
                    peer = %peer,
                    "tcp session spawned",
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_tcp",
                    session = %session_name,
                    peer = %peer,
                    error = ?error,
                    "tcp session spawn failed; stream closed during rollback",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn early_connection_waits_behind_the_activation_gate() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind gated listener");
        let addr = listener.local_addr().expect("gated listener address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_loop = Arc::clone(&shutdown);
        let (connection_tx, connection_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            run_accept_loop(listener, shutdown_for_loop, connection_tx, start_rx, || {});
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
        let thread = thread::spawn(move || {
            run_accept_loop(listener, shutdown, connection_tx, start_rx, || {});
        });

        drop(start_tx);
        thread.join().expect("dropping the pre-wire gate joins without a socket wake");
    }
}
