//! A signal-blocking [`DriverCapability`] for chassis whose main thread has
//! nothing to run.
//!
//! `run` blocks the main thread until SIGINT or SIGTERM arrives (Ctrl-C off
//! Unix), then drops the `SubstrateBoot` so the actor registry tears down. The
//! hub and the Bloomery compose it as their `Chassis::Driver`; headless polls a
//! shutdown flag from its tick loop and desktop quits its winit loop, so both
//! keep their own shutdown paths.
//!
//! Signal handling is sync: there is no async runtime to host. On Unix
//! `signal-hook`'s iterator API blocks the driver thread until SIGINT
//! or SIGTERM arrives; on Windows the `ctrlc` fallback covers Ctrl-C.

use std::marker::PhantomData;
use std::thread;

use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot, engine_name};

/// Driver capability that owns the `SubstrateBoot` whose registry hosts the
/// chassis actors. `run` blocks the calling thread on a SIGINT/SIGTERM
/// signal, then drops the boot so the actor registry tears down. The chassis
/// parameter names the binary in the log lines ([`engine_name`]).
pub struct SignalDriverCapability<C> {
    boot: SubstrateBoot,
    chassis: PhantomData<fn() -> C>,
}

impl<C> SignalDriverCapability<C> {
    /// Wrap the boot the chassis composed its actors on.
    #[must_use]
    pub const fn new(boot: SubstrateBoot) -> Self {
        Self { boot, chassis: PhantomData }
    }
}

/// Post-boot handle for [`SignalDriverCapability`].
pub struct SignalDriverRunning<C> {
    boot: SubstrateBoot,
    chassis: PhantomData<fn() -> C>,
}

impl<C: Chassis> DriverCapability for SignalDriverCapability<C> {
    type Running = SignalDriverRunning<C>;

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self { boot, chassis } = self;
        Ok(SignalDriverRunning { boot, chassis })
    }
}

impl<C: Chassis> DriverRunning for SignalDriverRunning<C> {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let Self { boot, .. } = *self;
        let engine = engine_name::<C>();
        let sig = shutdown_signal(&engine);
        tracing::info!("{engine}: {sig} received, shutting down");
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
/// SIGTERM. Ignoring SIGTERM means `pkill -f {engine}`
/// kills the engine without running drops.
#[cfg(unix)]
fn shutdown_signal(engine: &str) -> &'static str {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                "{engine}: signal handler install failed: {e}; \
                 parking thread — SIGKILL is the only exit"
            );
            thread::park();
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
fn shutdown_signal(engine: &str) -> &'static str {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<()>();
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = tx.send(());
    }) {
        tracing::error!(
            "{engine}: ctrl-c handler install failed: {e}; \
             parking thread — SIGKILL is the only exit"
        );
        thread::park();
        return "park";
    }
    let _ = rx.recv();
    "Ctrl-C"
}
