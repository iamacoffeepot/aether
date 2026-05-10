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

use crate::hub::HubClient;
use aether_actor::Actor;
use aether_capabilities::InputCapability;
use aether_data::{KindId, encode_empty, mailbox_id_from_name};
use aether_kinds::Tick;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{
    Mailer, SubstrateBoot, chassis::frame_loop, mail::MailboxId,
    runtime::trace::push_chassis_root_mail,
};

/// Wire-stable `EngineInfo.workers` value (ADR-0038: post actor-per-
/// component, the scheduler doesn't read this — it's retained on the
/// hub-protocol wire for compatibility). The shared frame-loop
/// policy (drain budget, frame-stats cadence) lives in
/// `aether_substrate::frame_loop`.
pub const WORKERS: usize = 2;
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
    pub kind_frame_stats: KindId,
    pub tick_period: Duration,
    /// Held for the chassis lifetime so the hub reader + heartbeat
    /// threads stay spawned. `None` when `AETHER_HUB_URL` was unset.
    pub hub: Option<HubClient>,
}

pub struct HeadlessTimerRunning {
    queue: Arc<Mailer>,
    /// `aether.input` mailbox id, cached at boot. Each generated tick
    /// pushes one mail here and the input cap fans out per
    /// subscriber (issue 640).
    input_mailbox: MailboxId,
    kind_tick: KindId,
    kind_frame_stats: KindId,
    tick_period: Duration,
    /// `SubstrateBoot` drops at the end of `run()` so its scheduler
    /// joins workers before the hub disconnects.
    _boot: SubstrateBoot,
    _hub: Option<HubClient>,
}

impl DriverCapability for HeadlessTimerCapability {
    type Running = HeadlessTimerRunning;

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let HeadlessTimerCapability {
            boot,
            kind_tick,
            kind_frame_stats,
            tick_period,
            hub,
        } = self;

        Ok(HeadlessTimerRunning {
            queue: Arc::clone(&boot.queue),
            input_mailbox: mailbox_id_from_name(InputCapability::NAMESPACE),
            kind_tick,
            kind_frame_stats,
            tick_period,
            _boot: boot,
            _hub: hub,
        })
    }
}

impl DriverRunning for HeadlessTimerRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let HeadlessTimerRunning {
            queue,
            input_mailbox,
            kind_tick,
            kind_frame_stats,
            tick_period,
            _boot,
            _hub,
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

        let started = Instant::now();
        let mut frame: u64 = 0;
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

            frame += 1;
            push_chassis_root_mail(
                &queue,
                next_correlation(),
                input_mailbox,
                kind_tick,
                encode_empty::<Tick>(),
                1,
            );
            if frame.is_multiple_of(frame_loop::LOG_EVERY_FRAMES) {
                frame_loop::emit_frame_stats(
                    &queue,
                    kind_frame_stats,
                    frame,
                    0,
                    next_correlation(),
                );
                let elapsed = started.elapsed().as_secs_f64().max(0.001);
                tracing::info!(
                    target: "aether_substrate::frame_loop",
                    frame = frame,
                    fps = frame as f64 / elapsed,
                    "headless tick",
                );
            }
        }
        // The `loop` above never breaks — process exit is the only
        // termination path (SIGTERM/SIGINT or `fatal_abort`).
    }
}
