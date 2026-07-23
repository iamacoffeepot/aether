use winit::event_loop::EventLoopProxy;

use crate::chassis::UserEvent;

/// Install a SIGINT/SIGTERM → graceful-shutdown bridge for the desktop
/// chassis (iamacoffeepot/aether#1489). On the first delivered signal it sends
/// [`UserEvent::Quit`] through `proxy`; the window-owned application turns
/// that event directly into graceful lifecycle shutdown.
///
/// Unlike headless's `signal_hook::flag::register` — which is
/// async-signal-safe but can only flip a bool — the desktop loop must be
/// *woken*, and `EventLoopProxy::send_event` is not async-signal-safe.
/// So a dedicated watcher thread blocks on the signal stream and sends the
/// proxy event; it doesn't freeze the winit loop. SIGTERM joins SIGINT so
/// supervisors / `kill` (no `-9`) / CI cancellation also run teardown.
/// Best-effort: a failed install warn-logs and leaves shutdown to native
/// window close.
#[cfg(unix)]
pub(super) fn install_shutdown_handler(proxy: EventLoopProxy<UserEvent>) {
    use std::thread;

    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                target: "aether_substrate::boot",
                error = %e,
                "desktop: shutdown signal handler install failed; \
                 only window-close will trigger graceful shutdown",
            );
            return;
        }
    };
    // Infra thread: it blocks on the OS signal stream and holds no
    // settlement/trace contract — the work it triggers (the `Quit` push)
    // happens later, on the winit main thread, through the normal mail
    // path. A separate thread (not the single-threaded winit loop), so
    // it never freezes the loop.
    #[allow(clippy::disallowed_methods)]
    let spawned = thread::Builder::new().name("aether-desktop-signal".into()).spawn(move || {
        // The first signal begins graceful shutdown; the iterator
        // only ends if the underlying fd closes (it doesn't for the
        // thread's lifetime), so a single `next()` is the whole job.
        if signals.forever().next().is_some() {
            let _ = proxy.send_event(UserEvent::Quit);
        }
    });
    if let Err(e) = spawned {
        tracing::error!(
            target: "aether_substrate::boot",
            error = %e,
            "desktop: shutdown signal-watcher thread failed to spawn; \
             only window-close will trigger graceful shutdown",
        );
    }
}

#[cfg(not(unix))]
pub(super) fn install_shutdown_handler(proxy: EventLoopProxy<UserEvent>) {
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = proxy.send_event(UserEvent::Quit);
    }) {
        tracing::error!(
            target: "aether_substrate::boot",
            error = %e,
            "desktop: ctrl-c handler install failed; \
             only window-close will trigger graceful shutdown",
        );
    }
}
