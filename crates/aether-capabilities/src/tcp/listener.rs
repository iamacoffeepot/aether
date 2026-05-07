//! `aether.tcp.listener` — instanced actor, one per bound port. Owns
//! a `std::net::TcpListener` and a sidecar accept thread that loops
//! on `accept()`. Phase 6a drops accepted streams; Phase 6b spawns a
//! `TcpSessionActor` per accepted connection.
//!
//! Shutdown: `on_close` flips the accept thread's shutdown flag, then
//! self-connects to the bound port to wake the blocked accept call.
//! The accept returns, sees the flag, breaks; the dispatcher thread
//! (in `on_close`) joins the accept thread.

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::Close;

// `TcpListenerConfig` carries `std::net::TcpListener` (native-only) so
// it lives inside the bridge mod. Re-export at file root for the cap
// module to consume.
#[cfg(not(target_arch = "wasm32"))]
pub use listener_native::TcpListenerConfig;

#[aether_actor::bridge(instanced)]
mod listener_native {
    use super::Close;
    use aether_actor::actor;
    use aether_substrate::capability::BootError;
    use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

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

    /// Issue 629 / Phase B: `accept_thread` is a plain `Option`.
    /// `shutdown` stays `Arc<AtomicBool>` because it's genuinely
    /// cross-thread shared with the sidecar accept thread.
    pub struct TcpListenerActor {
        local_port: u16,
        shutdown: Arc<AtomicBool>,
        accept_thread: Option<JoinHandle<()>>,
    }

    #[actor]
    impl NativeActor for TcpListenerActor {
        type Config = TcpListenerConfig;
        const NAMESPACE: &'static str = "aether.tcp.listener";

        fn init(
            mut config: TcpListenerConfig,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            let listener = config
                .listener
                .take()
                .expect("TcpListenerConfig::listener consumed exactly once");
            let addr = config.addr;
            let port = config.port;
            // Stay blocking — the accept loop wakes via self-connect
            // on `on_close`. Nonblocking would require a poll loop +
            // CPU burn for no win.
            listener
                .set_nonblocking(false)
                .map_err(|e| BootError::Other(Box::new(e)))?;
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_for_thread = Arc::clone(&shutdown);
            let thread = std::thread::Builder::new()
                .name(format!("aether-tcp-accept-{port}"))
                .spawn(move || {
                    while !shutdown_for_thread.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _peer)) => {
                                if shutdown_for_thread.load(Ordering::Acquire) {
                                    drop(stream);
                                    break;
                                }
                                // Phase 6a: close the stream.
                                // Phase 6b spawns TcpSessionActor.
                                drop(stream);
                            }
                            Err(_) => {
                                if shutdown_for_thread.load(Ordering::Acquire) {
                                    break;
                                }
                                continue;
                            }
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
            })
        }

        fn on_close(&mut self, _ctx: &mut NativeCtx<'_>) {
            self.shutdown.store(true, Ordering::Release);
            // Wake the blocked accept(). Self-connect to the bound
            // port; the accept returns, sees the flag, breaks. Short
            // connect timeout so a misconfigured listener (port
            // unreachable) doesn't hang the close path.
            let addr_str = format!("127.0.0.1:{}", self.local_port);
            if let Ok(addr) = addr_str.parse::<std::net::SocketAddr>() {
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
        /// drains, runs `on_close`, and the close fan-out fires
        /// `MonitorNotice` to the cap.
        #[handler]
        fn on_close_request(&mut self, ctx: &mut NativeCtx<'_>, _mail: Close) {
            ctx.shutdown();
        }
    }
}
