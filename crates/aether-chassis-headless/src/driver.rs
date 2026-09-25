//! Headless chassis driver capability — ADR-0071 phase 5.
//!
//! Wraps the std-timer tick loop in a [`DriverCapability`] so the
//! headless chassis composes the same way as desktop: passive
//! capabilities + exactly one driver. The driver's `run()` body
//! holds what was previously `HeadlessChassis::run` — a fixed-cadence
//! tick generator (default 60 Hz, `AETHER_TICK_HZ` override) that
//! pumps `Tick` mail to subscribed mailboxes, drains the mail queue,
//! and emits frame-stats observation every
//! `frame_loop::LOG_EVERY_FRAMES` frames.
//!
//! No `Send` bound on the driver capability or its running — the
//! headless tick loop runs on the chassis main thread end-to-end (no
//! winit, but the `chassis_builder`'s single-threaded
//! Builder→BuiltChassis→run path applies all the same).
//!
//! A SIGINT/SIGTERM shutdown flag (`signal_hook::flag::register` on
//! Unix, `ctrlc` on Windows) lets the loop break so `run()` returns and
//! the chassis teardown unwinds — per-actor `unwire`, `lock.pid` removal
//! (ADR-0049 §7), and the `index.bin` boot-snapshot — the headless
//! analogue of desktop returning from winit's `event_loop.run_app`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_kinds::LifecycleAdvance;
use aether_lifecycle::LifecycleCapability;
use aether_substrate::SubstrateBoot;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RootPusher, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::{ConfigMember, ConfigMemberRecord};

/// ADR-0071 driver capability for the headless chassis. Owns the
/// pieces the timer loop needs at construction time, then `boot()`
/// captures them on a [`HeadlessTimerRunning`] that drives the loop.
///
/// The timer fires `LifecycleAdvance` at `aether.lifecycle`, and the
/// `LifecycleCapability` owns the broadcast vocabulary so the substrate
/// observes a labelled `aether.lifecycle` root for every frame chain.
pub struct HeadlessTimerDriverCapability {
    pub boot: SubstrateBoot,
    pub tick_period: Duration,
}

pub struct HeadlessTimerRunning {
    /// The chassis-root door to `aether.lifecycle`, minted at boot. Each
    /// tick fires one `LifecycleAdvance` through it; the lifecycle driver
    /// broadcasts the current stage (Tick) directly to its stage subscriber
    /// set (components subscribe `Tick` on `aether.lifecycle`).
    lifecycle: RootPusher<LifecycleCapability>,
    tick_period: Duration,
    /// SIGINT/SIGTERM shutdown flag, flipped from the signal handler
    /// installed in [`HeadlessTimerDriverCapability::boot`]. The run loop
    /// checks it at the top of each iteration and `break`s, so `run()`
    /// returns and the chassis teardown unwinds. A struct field (not a
    /// loop-local) so tests can inject a pre-set flag and drive `run()`
    /// to a clean return without sending a real signal.
    shutdown: Arc<AtomicBool>,
    /// `SubstrateBoot` drops at the end of `run()` so its scheduler
    /// joins workers before the chassis exits.
    _boot: SubstrateBoot,
}

impl DriverCapability for HeadlessTimerDriverCapability {
    type Running = HeadlessTimerRunning;

    /// ADR-0156 §4: the tick cadence knob (`AETHER_TICK_HZ`) belongs to the
    /// headless timer driver — the driver that owns the std-timer loop — so
    /// the chassis config aggregate carries it only where a timer composes it
    /// (desktop, which drives from winit, declares no tick knob).
    fn config_members() -> Vec<ConfigMemberRecord> {
        <aether_chassis::TickConfig as ConfigMember>::members()
    }

    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self { boot, tick_period } = self;

        let shutdown = Arc::new(AtomicBool::new(false));
        install_shutdown_handler(&shutdown);

        Ok(HeadlessTimerRunning {
            lifecycle: ctx.root_pusher::<LifecycleCapability>(),
            tick_period,
            shutdown,
            _boot: boot,
        })
    }
}

/// Install a SIGINT/SIGTERM → `shutdown` flag handler so the tick loop
/// can break and `run()` return, letting the chassis teardown unwind
/// (per-actor `unwire`, `lock.pid` removal, the `index.bin` snapshot).
/// `signal_hook::flag::register` flips the `AtomicBool` directly from the
/// async-signal-safe handler — no watcher thread, since the loop already
/// polls the flag every tick (rejected the hub's blocking
/// `signals.forever()`, which would freeze ticks).
///
/// Both signals on Unix: interactive shells deliver SIGINT, but process
/// supervisors (systemd), `pkill` / `kill` (no `-9`), and CI cancellation
/// send SIGTERM; ignoring it would skip teardown the way `SIGKILL` does.
/// Best-effort per ADR-0049 §7 — a failed install warn-logs and leaves
/// the loop running until the process is killed.
#[cfg(unix)]
fn install_shutdown_handler(shutdown: &Arc<AtomicBool>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::flag;
    for sig in [SIGINT, SIGTERM] {
        if let Err(e) = flag::register(sig, Arc::clone(shutdown)) {
            tracing::error!(
                target: "aether_substrate::boot",
                signal = sig,
                error = %e,
                "headless: shutdown signal handler install failed; \
                 teardown will be skipped when this signal arrives",
            );
        }
    }
}

#[cfg(not(unix))]
fn install_shutdown_handler(shutdown: &Arc<AtomicBool>) {
    let flag = Arc::clone(shutdown);
    if let Err(e) = ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    }) {
        tracing::error!(
            target: "aether_substrate::boot",
            error = %e,
            "headless: ctrl-c handler install failed; \
             teardown will be skipped on Ctrl-C",
        );
    }
}

impl DriverRunning for HeadlessTimerRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let Self {
            lifecycle,
            tick_period,
            shutdown,
            // Held to the end of `run()` so the scheduler joins workers on
            // drop; the `_` prefix keeps the binding alive without a use.
            _boot,
        } = *self;

        let delta_micros = u32::try_from(tick_period.as_micros()).unwrap_or(u32::MAX);

        let mut next_deadline = Instant::now() + tick_period;
        // Checked at the top of each iteration so a SIGINT/SIGTERM
        // observed during the prior tick's sleep breaks within one tick
        // period (~16 ms at 60 Hz) — fine for shutdown.
        while !shutdown.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next_deadline {
                thread::sleep(next_deadline - now);
            }
            // Catch the deadline up from the current instant rather
            // than the prior target — if a frame overruns (component
            // deliver stalled, hub socket flushed slowly) we resume
            // from now + period instead of trying to burn through
            // backlog, which would just compound the stall.
            next_deadline = Instant::now() + tick_period;

            // Fire-and-forget LifecycleAdvance. The driver's settlement
            // gating tracks one pending advance at a time — frames that
            // overlap (settlement still pending when the next deadline
            // hits) warn-drop at the driver per ADR-0082 §6.
            lifecycle.push_root(&LifecycleAdvance { delta_micros }, None);
        }

        // SIGINT/SIGTERM flipped `shutdown` (or a test pre-set it): the
        // loop broke, so `run()` returns. The destructured locals drop —
        // `boot` joins the scheduler workers — the teardown a bare
        // SIGKILL would skip.
        Ok(())
    }
}
