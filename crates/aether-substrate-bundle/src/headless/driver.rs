//! Headless chassis driver capability — ADR-0071 phase 5.
//!
//! Wraps the std-timer tick loop in a [`DriverCapability`] so the
//! headless chassis composes the same way as desktop: passive
//! capabilities + exactly one driver. The driver's `run()` body
//! holds what was previously `HeadlessChassis::run` — a fixed-cadence
//! tick generator (default 60 Hz, `AETHER_TICK_HZ` override) that
//! pumps `Tick` mail to subscribed mailboxes, drains the mail queue,
//! and emits frame-stats observation every
//! [`frame_loop::LOG_EVERY_FRAMES`] frames.
//!
//! No `Send` bound on the driver capability or its running — the
//! headless tick loop runs on the chassis main thread end-to-end (no
//! winit, but the chassis_builder's single-threaded
//! Builder→BuiltChassis→run path applies all the same).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Actor;
use aether_capabilities::InputCapability;
use aether_data::{KindId, encode_empty, mailbox_id_from_name};
use aether_kinds::Tick;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{
    Mailer, SubstrateBoot, mail::MailboxId, runtime::trace::push_chassis_root_mail,
};

pub const DEFAULT_TICK_HZ: u32 = 60;

/// Parse `AETHER_TICK_HZ`. Unset → [`DEFAULT_TICK_HZ`]; non-positive
/// or unparseable → log + fall back to default. Tests bypass this by
/// constructing `HeadlessEnv` with a chosen `tick_period` directly.
pub fn parse_tick_hz_env() -> u32 {
    match std::env::var("AETHER_TICK_HZ") {
        Ok(s) => s
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&hz| hz > 0)
            .unwrap_or_else(|| {
                tracing::warn!(
                    target: "aether_substrate::boot",
                    value = %s,
                    "AETHER_TICK_HZ unparseable or zero — falling back to default",
                );
                DEFAULT_TICK_HZ
            }),
        Err(_) => DEFAULT_TICK_HZ,
    }
}

/// ADR-0071 driver capability for the headless chassis. Owns the
/// pieces the timer loop needs at construction time, then `boot()`
/// captures them on a [`HeadlessTimerRunning`] that drives the loop.
pub struct HeadlessTimerCapability {
    pub boot: SubstrateBoot,
    pub kind_tick: KindId,
    pub tick_period: Duration,
}

pub struct HeadlessTimerRunning {
    queue: Arc<Mailer>,
    /// `aether.input` mailbox id, cached at boot. Each generated tick
    /// pushes one mail here and the input cap fans out per
    /// subscriber (issue 640).
    input_mailbox: MailboxId,
    kind_tick: KindId,
    tick_period: Duration,
    /// `SubstrateBoot` drops at the end of `run()` so its scheduler
    /// joins workers before the chassis exits.
    _boot: SubstrateBoot,
}

impl DriverCapability for HeadlessTimerCapability {
    type Running = HeadlessTimerRunning;

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let HeadlessTimerCapability {
            boot,
            kind_tick,
            tick_period,
        } = self;

        Ok(HeadlessTimerRunning {
            queue: Arc::clone(&boot.queue),
            input_mailbox: mailbox_id_from_name(InputCapability::NAMESPACE),
            kind_tick,
            tick_period,
            _boot: boot,
        })
    }
}

impl DriverRunning for HeadlessTimerRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let HeadlessTimerRunning {
            queue,
            input_mailbox,
            kind_tick,
            tick_period,
            _boot,
        } = *self;

        // ADR-0080 §6 chassis-root correlation counter (issue
        // iamacoffeepot/aether#723). One per driver, symmetric with the
        // per-actor counter on `NativeBinding`. Skipping 0 keeps the
        // sentinel slot reserved.
        let chassis_correlation = AtomicU64::new(1);
        let next_correlation = || -> u64 {
            let id = chassis_correlation.fetch_add(1, Ordering::Relaxed);
            if id == 0 {
                chassis_correlation.fetch_add(1, Ordering::Relaxed)
            } else {
                id
            }
        };

        let mut next_deadline = Instant::now() + tick_period;
        loop {
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

            push_chassis_root_mail(
                &queue,
                next_correlation(),
                input_mailbox,
                kind_tick,
                encode_empty::<Tick>(),
                1,
            );
        }
        // The `loop` above never breaks — process exit is the only
        // termination path (SIGTERM/SIGINT or `fatal_abort`).
    }
}
