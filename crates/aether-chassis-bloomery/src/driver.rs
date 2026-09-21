//! Bloomery chassis driver capability — a signal-blocking [`DriverCapability`]
//! with no tick.
//!
//! The bloomery is journal-driven: the bundle driver wakes on `WatchHead`, and
//! nothing needs `LifecycleAdvance`. `run` blocks the main thread until
//! SIGINT/SIGTERM, then drops the boot so the actor registry tears down and the
//! driver's `Drop` abandons parked replies. Follows
//! `HubServerDriverCapability`, which cannot be reused — its `boot` field is
//! private and it lives in the hub crate.
//!
//! Signal handling is sync: there is no async runtime to host. On Unix
//! `signal-hook`'s iterator API blocks the driver thread until SIGINT
//! or SIGTERM arrives; on Windows the `ctrlc` fallback covers Ctrl-C.

use aether_substrate::SubstrateBoot;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;

/// Driver capability for the bloomery chassis. Owns the `SubstrateBoot` whose
/// registry hosts the journal owner, the bundle driver, and the composed caps.
/// `run` blocks the calling thread on a SIGINT/SIGTERM signal, then drops
/// the boot so the actor registry tears down.
pub struct BloomeryDriverCapability {
    pub boot: SubstrateBoot,
}

/// Post-boot handle for [`BloomeryDriverCapability`].
pub struct BloomeryDriverRunning {
    boot: SubstrateBoot,
}

impl DriverCapability for BloomeryDriverCapability {
    type Running = BloomeryDriverRunning;

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self { boot } = self;
        Ok(BloomeryDriverRunning { boot })
    }
}

impl DriverRunning for BloomeryDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let Self { boot } = *self;
        let sig = shutdown_signal();
        tracing::info!("aether-bloomery: {sig} received, shutting down");
        // `boot` drops here — actor registries shut down, dispatcher
        // threads see their inbox senders drop and exit.
        drop(boot);
        Ok(())
    }
}

/// Blocks the calling thread until SIGINT or SIGTERM arrives on Unix;
/// on Windows falls back to Ctrl-C only via `ctrlc`. Returns a short
/// label for the log line.
///
/// Why both signals on Unix: interactive shells deliver SIGINT, but
/// process supervisors (systemd, supervisord), shell utilities
/// (`pkill`, `kill` without `-9`), and CI cancellation all send
/// SIGTERM. Ignoring SIGTERM means `pkill -f aether-bloomery`
/// kills the engine without running drops.
#[cfg(unix)]
fn shutdown_signal() -> &'static str {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(signals) => signals,
        Err(error) => {
            tracing::error!(
                "aether-bloomery: signal handler install failed: {error}; \
                 parking thread — SIGKILL is the only exit"
            );
            std::thread::park();
            return "park";
        }
    };
    // The iterator only returns `None` if the underlying file
    // descriptor closes — can't happen for the lifetime of `signals`,
    // but the explicit branch keeps coverage total.
    match signals.forever().next() {
        Some(SIGINT) => "SIGINT",
        Some(SIGTERM) => "SIGTERM",
        Some(_) => "unknown signal",
        None => "signal stream ended",
    }
}

#[cfg(not(unix))]
fn shutdown_signal() -> &'static str {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<()>();
    if let Err(error) = ctrlc::set_handler(move || {
        let _ = tx.send(());
    }) {
        tracing::error!(
            "aether-bloomery: ctrl-c handler install failed: {error}; \
             parking thread — SIGKILL is the only exit"
        );
        std::thread::park();
        return "park";
    }
    let _ = rx.recv();
    "Ctrl-C"
}
