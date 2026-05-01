//! ADR-0070 Phase 3 (part 3): network egress sink as a native
//! capability.
//!
//! Wraps the ADR-0043 `aether.sink.net` mailbox: components mail
//! `aether.net.fetch` here, the capability decodes through
//! `net::dispatch_net_mail`, drives the `NetAdapter` (typically
//! `UreqNetAdapter`), and replies via `Mailer::send_reply`.
//!
//! State held by the capability: an `Arc<dyn NetAdapter>` configured
//! at boot from the [`NetConfig`] passed in by the chassis main, plus
//! the default-timeout the dispatcher applies when a `Fetch` request
//! omits `timeout_ms`. Adapter construction is infallible
//! (`build_net_adapter` returns `Arc<dyn NetAdapter>` directly), so
//! there's no fail-fast ergonomics question — boot can't return an
//! error today.
//!
//! Threading: single dispatcher thread, `AtomicBool` +
//! `recv_timeout(100ms)` shutdown — same shape as the other
//! capabilities. The dispatcher's `fetch` call blocks on the network
//! synchronously; ADR-0043 §2 flags this as the head-of-line blocking
//! source to fix in a future multi-threaded dispatcher ADR.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::capability::{BootError, Capability, ChassisCtx, RunningCapability};
use crate::net::{self, NetConfig};

/// Recipient name the net capability claims. ADR-0058 places
/// chassis-owned sinks under `aether.sink.*`.
pub const NET_SINK_NAME: &str = "aether.sink.net";

/// Polling interval for the dispatcher's shutdown check.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Native capability owning the ADR-0043 net-egress sink. Constructor
/// takes a [`NetConfig`] (resolved from env or built explicitly by
/// the chassis main per issue 464).
pub struct NetCapability {
    config: NetConfig,
}

impl NetCapability {
    pub fn new(config: NetConfig) -> Self {
        Self { config }
    }
}

/// Running handle returned by [`NetCapability::boot`]. Same shape as
/// the other dispatcher-thread capabilities.
pub struct NetRunning {
    thread: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl Capability for NetCapability {
    type Running = NetRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let claim = ctx.claim_mailbox(NET_SINK_NAME)?;
        let mailer = ctx.mail_send_handle();
        let default_timeout = self.config.default_timeout;
        let adapter = net::build_net_adapter(self.config);

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let thread_flag = Arc::clone(&shutdown_flag);
        let receiver = claim.receiver;

        let thread = thread::Builder::new()
            .name("aether-net-sink".into())
            .spawn(move || {
                while !thread_flag.load(Ordering::Relaxed) {
                    match receiver.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                        Ok(env) => {
                            net::dispatch_net_mail(
                                adapter.as_ref(),
                                &mailer,
                                env.kind,
                                env.sender,
                                &env.payload,
                                default_timeout,
                            );
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(NetRunning {
            thread: Some(thread),
            shutdown_flag,
        })
    }
}

impl RunningCapability for NetRunning {
    fn shutdown(self: Box<Self>) {
        let NetRunning {
            mut thread,
            shutdown_flag,
        } = *self;
        shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(t) = thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::mailer::Mailer;
    use crate::registry::Registry;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        (registry, Arc::new(Mailer::new()))
    }

    /// Boot the capability against a default disabled NetConfig and
    /// confirm the sink is registered. The dispatch path itself is
    /// exercised by `net::tests` against the same `dispatch_net_mail`.
    #[test]
    fn capability_boots_and_registers_sink() {
        let (registry, mailer) = fresh_substrate();
        let config = NetConfig {
            disabled: true,
            ..NetConfig::default()
        };
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(NetCapability::new(config))
            .build()
            .expect("net capability boots");
        assert!(
            registry.lookup(NET_SINK_NAME).is_some(),
            "sink mailbox registered"
        );
        chassis.shutdown();
    }

    /// Builder rejects a duplicate claim. Same protection as the
    /// other capabilities.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let (registry, mailer) = fresh_substrate();
        registry.register_sink(NET_SINK_NAME, Arc::new(|_, _, _, _, _, _| {}));
        let config = NetConfig {
            disabled: true,
            ..NetConfig::default()
        };

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(NetCapability::new(config))
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name } if name == NET_SINK_NAME
        ));
    }
}
