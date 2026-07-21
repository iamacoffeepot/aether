//! The Bloomery driver: a long-running service that blocks on SIGINT/SIGTERM,
//! then drops the boot so the actor registry tears down (mirrors the hub
//! driver — Bloomery is a coordinator, not a frame loop).

use aether_substrate::SubstrateBoot;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;

/// The driver capability, owning the boot until the chassis runs.
pub struct BloomeryDriverCapability {
    /// The substrate boot the running half drops on shutdown.
    pub boot: SubstrateBoot,
}

/// The running driver — blocks in [`run`](DriverRunning::run) until a shutdown
/// signal, then drops the boot.
pub struct BloomeryDriverRunning {
    boot: SubstrateBoot,
}

impl DriverCapability for BloomeryDriverCapability {
    type Running = BloomeryDriverRunning;

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        Ok(BloomeryDriverRunning { boot: self.boot })
    }
}

impl DriverRunning for BloomeryDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let Self { boot } = *self;
        let signal = shutdown_signal();
        tracing::info!("aether-chassis-bloomery: {signal} received, shutting down");
        // Dropping the boot shuts the actor registries down and joins the
        // dispatcher threads — the whole chassis tears down here.
        drop(boot);
        Ok(())
    }
}

/// Block until a shutdown signal arrives, returning its name for the log.
#[cfg(unix)]
fn shutdown_signal() -> &'static str {
    use std::thread;

    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(signals) => signals,
        Err(error) => {
            tracing::error!(
                "aether-chassis-bloomery: signal handler install failed: {error}; \
                 parking thread — SIGKILL is the only exit"
            );
            // `park` can wake spuriously, so loop it: this path must never
            // return and drop the boot, matching the doc-comment contract that
            // it parks until the process is killed.
            loop {
                thread::park();
            }
        }
    };
    match signals.forever().next() {
        Some(SIGINT) => "SIGINT",
        Some(SIGTERM) => "SIGTERM",
        Some(_) => "unknown signal",
        None => "signal stream ended",
    }
}

/// Non-unix fallback: park until the process is killed. Bloomery is a
/// linux-hosted coordinator; a richer Ctrl-C path is not needed for this slice.
#[cfg(not(unix))]
fn shutdown_signal() -> &'static str {
    use std::thread;

    // `park` can wake spuriously, so loop it: this path never returns — the
    // process exits only on a kill signal (the doc-comment contract). The
    // diverging `loop` coerces to the shared `&'static str` return.
    loop {
        thread::park();
    }
}
