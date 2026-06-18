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
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::{Close, ConnectionReady};

// `TcpListenerConfig` carries `std::net::TcpListener` (native-only) so
// it lives inside the bridge mod. Re-export at file root for the cap
// module to consume.
#[cfg(not(target_arch = "wasm32"))]
pub use listener_native::TcpListenerConfig;

#[aether_actor::bridge(instanced, one_per = "listener")]
mod listener_native {
    use super::{Close, ConnectionReady};
    use aether_actor::actor;
    use aether_data::Kind;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::{KindId, Mail, Mailer};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use crate::tcp::session::{TcpSessionActor, TcpSessionConfig};
    use std::thread;

    /// Init config for [`TcpListenerActor`]. `TcpCapability::on_bind`
    /// binds the socket on the dispatcher thread (so addr-parse / port-
    /// in-use failures surface synchronously) and hands the bound
    /// listener through `spawn_child`. The `listener` field is
    /// `Option` so init can move it out into the accept thread.
    pub struct TcpListenerConfig {
        pub listener: Option<TcpListener>,
        pub addr: String,
        pub port: u16,
    }

    /// Issue 607 Phase 6b: the accept thread can't call
    /// `ctx.spawn_child` (no dispatcher ctx), so it pushes accepted
    /// streams over `connection_rx` and fires a [`ConnectionReady`]
    /// wake mail. The dispatcher's `on_connection_ready` handler
    /// drains the mpsc and spawns one `TcpSessionActor` per pending
    /// stream.
    pub struct TcpListenerActor {
        local_port: u16,
        shutdown: Arc<AtomicBool>,
        accept_thread: Option<JoinHandle<()>>,
        connection_rx: mpsc::Receiver<(TcpStream, SocketAddr)>,
        next_subname: u64,
    }

    #[actor]
    impl NativeActor for TcpListenerActor {
        type Config = TcpListenerConfig;
        const NAMESPACE: &'static str = "aether.tcp.listener";

        fn init(
            mut config: TcpListenerConfig,
            ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
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

            Ok(Self {
                local_port: port,
                shutdown,
                accept_thread: Some(thread),
                connection_rx,
                next_subname: 0,
            })
        }

        fn unwire(&mut self, _ctx: &mut NativeCtx<'_>) {
            self.shutdown.store(true, Ordering::Release);
            // Wake the blocked accept(). Self-connect to the bound
            // port; the accept returns, sees the flag, breaks. Short
            // connect timeout so a misconfigured listener (port
            // unreachable) doesn't hang the close path.
            let addr_str = format!("127.0.0.1:{}", self.local_port);
            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
            }
            if let Some(thread) = self.accept_thread.take() {
                let _ = thread.join();
            }
            tracing::info!(
                target: "aether_substrate::tcp",
                port = self.local_port,
                "tcp listener closed",
            );
        }

        /// Cooperative external close. The unbind path on
        /// `TcpCapability` mails this; we shut down so the dispatcher
        /// drains, runs `unwire`, and the close fan-out fires
        /// `MonitorNotice` to the cap.
        // Stateless close request: `&mut self` rides the dispatch ABI;
        // shutdown is requested through `ctx`, not through any field.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_close_request(&mut self, ctx: &mut NativeCtx<'_>, _mail: Close) {
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
        fn on_connection_ready(&mut self, ctx: &mut NativeCtx<'_>, _mail: ConnectionReady) {
            while let Ok((stream, peer)) = self.connection_rx.try_recv() {
                let subname = format!("conn-{}", self.next_subname);
                self.next_subname += 1;
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
}
