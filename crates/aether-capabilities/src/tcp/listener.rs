//! `aether.tcp.listener` — instanced actor, one per bound port. Owns
//! a `std::net::TcpListener` and a sidecar accept thread that loops
//! on blocking `accept()`. Phase 6b: each accepted connection spawns
//! a `TcpSessionActor` as a child; the sidecar can't call
//! `spawn_child` (no dispatcher ctx), so it pushes the `TcpStream`
//! over an mpsc and fires a `ConnectionReady` wake mail at this
//! actor's own mailbox. The wake handler drains the mpsc and does
//! the spawn on the dispatcher thread.
//!
//! Shutdown: `unwire` flips the accept thread's shutdown flag, then
//! self-connects to the bound port to wake the blocked accept call.
//! The accept returns, sees the flag, breaks; the dispatcher thread
//! (in `unwire`) joins the accept thread.

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers against the
// identity (always-on, outside the `feature = "runtime"` gate).
use super::kinds::{Close, ConnectionReady};

/// `aether.tcp.listener` **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the instanced
/// `OnePer("listener")` name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`TcpListenerState`, which holds the
/// `std::net::TcpListener`'s accept thread + the connection channel) lives
/// behind the one `feature = "runtime"` gate, so a transport-only build never
/// names `TcpListenerState` nor pulls `aether_substrate` through this actor.
pub struct TcpListenerActor;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` / `std::net` type — the
// handler/init ctx, the runtime state, the accept thread — lives in the
// `runtime` module below, gated once by `feature = "runtime"` and reached
// through the single `use runtime::*` glob.
use aether_actor::actor;

#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

#[cfg(feature = "runtime")]
mod runtime;

#[actor(instanced, one_per = "listener")]
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
