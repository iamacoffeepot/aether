//! The restart-backoff timer sidecar.
//!
//! An engine death is observed on the dispatcher thread, but the restart
//! must not happen there and then: the policy's backoff is a settle
//! window, and sleeping it inside a handler would stall every other actor
//! the chassis is running. So the wait is pushed onto a thread and the
//! decision comes back as mail.
//!
//! The thread carries no recipe — only a token. What to re-fork stays in
//! [`FleetServerState::pending_restarts`](super::runtime::FleetServerState),
//! keyed by that token, so the recipe never has to survive a trip through
//! a wire kind and the timer stays a pure alarm clock.
//!
//! One-shot and detached, unlike the proxy's repeating heartbeat: there is
//! nothing to stop early and nothing to join. The timer holds a
//! [`SelfWake`], never the cap's mailbox position (ADR-0230), so a timer
//! that outlives its cap wakes nothing.

use crate::kinds::EngineRestartDue;
use aether_substrate::actor::native::SelfWake;
use std::thread;
use std::time::Duration;

/// Sleep `backoff`, then wake the engines cap to re-fork the restart
/// filed under `token`.
///
/// Fire-and-forget: the caller has already recorded the pending restart,
/// so the only thing that can be lost by a chassis teardown mid-sleep is
/// a restart whose cap is going away anyway.
pub fn schedule_restart(wake: SelfWake<EngineRestartDue>, token: u64, backoff: Duration) {
    // An infra timer below the mail layer, like the proxy's heartbeat
    // sidecar: it fires one wake-mail and exits, with no inbound chain to
    // inherit and so no settlement umbrella to honor.
    #[allow(clippy::disallowed_methods)] // aether-suppression-request: infra timer; one wake-mail then exits
    let spawned = thread::Builder::new().name("aether-fleet-restart".into()).spawn(move || {
        thread::sleep(backoff);
        wake.wake(&EngineRestartDue { token });
    });

    if let Err(e) = spawned {
        // The OS refused a thread. Say so at the level an operator will
        // see: the engine this token stood for is not coming back, and
        // its pending entry is now unreachable garbage in the cap's map.
        tracing::error!(
            target: "aether_substrate::fleet_server",
            token,
            error = %e,
            "engine restart: could not spawn the backoff timer; the engine will not be restarted",
        );
    }
}
