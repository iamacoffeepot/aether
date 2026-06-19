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

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_capabilities::LifecycleCapability;
use aether_data::{Kind, KindId, encode_empty, mailbox_id_from_name};
use aether_kinds::LifecycleAdvance;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Mailer, SubstrateBoot, mail::MailboxId};

use crate::chassis_root::next_chassis_correlation;

pub const DEFAULT_TICK_HZ: u32 = 60;

/// Parse `AETHER_TICK_HZ`. Unset → [`DEFAULT_TICK_HZ`]; non-positive
/// or unparseable → log + fall back to default. Tests bypass this by
/// constructing `HeadlessEnv` with a chosen `tick_period` directly.
#[must_use]
// Headless chassis boot config: the AETHER_TICK_HZ tick-cadence override read at
// the process boundary — not a cap config knob.
#[allow(clippy::disallowed_methods)]
pub fn parse_tick_hz_env() -> u32 {
    // Match arms read cleaner than `map_or` here because the Ok arm
    // is a chained iterator/closure that warn-logs on parse failure.
    #[allow(clippy::option_if_let_else)]
    match env::var("AETHER_TICK_HZ") {
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
///
/// Pre-ADR-0082 this drove `Tick` mail directly to `aether.input`;
/// it now fires `LifecycleAdvance` at `aether.lifecycle`, and the
/// `LifecycleCapability` owns the broadcast vocabulary so the substrate
/// observes a labelled `aether.lifecycle` root for every frame chain.
pub struct HeadlessTimerDriverCapability {
    pub boot: SubstrateBoot,
    /// Field kept for wire compatibility; the timer body no longer
    /// touches `Tick` directly post-ADR-0082, but chassis builders
    /// resolve the kind id from `SubstrateBoot::registry` once and
    /// hand it through this struct. Removing the field would touch
    /// every chassis call site; left as a no-op.
    #[allow(dead_code)]
    pub kind_tick: KindId,
    pub tick_period: Duration,
}

pub struct HeadlessTimerRunning {
    queue: Arc<Mailer>,
    /// `aether.lifecycle` mailbox id, cached at boot. Each tick fires
    /// one `LifecycleAdvance` here; the lifecycle driver broadcasts the
    /// current stage (Tick) directly to its stage subscriber set
    /// (issue 1490 retired the `Tick → aether.input` relay; components
    /// subscribe `Tick` on `aether.lifecycle`).
    lifecycle_mailbox: MailboxId,
    /// Kind id of [`LifecycleAdvance`], pre-resolved so the timer
    /// loop body stays alloc-free per tick.
    kind_lifecycle_advance: KindId,
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

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self {
            boot,
            kind_tick: _,
            tick_period,
        } = self;

        let shutdown = Arc::new(AtomicBool::new(false));
        install_shutdown_handler(&shutdown);

        Ok(HeadlessTimerRunning {
            queue: Arc::clone(&boot.queue),
            // Chassis route-freezing: the lifecycle cap's own id (its NAMESPACE),
            // ctx-less, no sibling resolver in scope.
            #[allow(clippy::disallowed_methods)]
            lifecycle_mailbox: mailbox_id_from_name(
                <LifecycleCapability as Addressable>::NAMESPACE,
            ),
            kind_lifecycle_advance: <LifecycleAdvance as Kind>::ID,
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
            queue,
            lifecycle_mailbox,
            kind_lifecycle_advance,
            tick_period,
            shutdown,
            // Bound (not `_boot`) so the teardown snapshot below can reach
            // the handle store; still held to the end of `run()` so the
            // scheduler joins workers on drop.
            _boot: boot,
        } = *self;

        // ADR-0080 §6 chassis-root correlation counter (issue
        // iamacoffeepot/aether#723). One per driver, symmetric with the
        // per-actor counter on `NativeBinding`. Skipping 0 keeps the
        // sentinel slot reserved.
        let chassis_correlation = AtomicU64::new(1);

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
            queue.push_chassis_root_mail(
                next_chassis_correlation(&chassis_correlation),
                lifecycle_mailbox,
                kind_lifecycle_advance,
                encode_empty::<LifecycleAdvance>(),
                1,
            );
        }

        // SIGINT/SIGTERM flipped `shutdown` (or a test pre-set it): the
        // loop broke, so `run()` returns. ADR-0049 §3 boot fast-path
        // (issue #1446): write the `index.bin` snapshot so the next boot
        // loads it in one read + decode instead of one `open()` per
        // `.meta` sidecar; best-effort + a no-op when persistence is
        // disabled. Symmetric to the desktop seam after `run_app`. Then
        // the destructured locals drop — `boot` joins the scheduler
        // workers and `Drop for HandleStore` removes `lock.pid`
        // (ADR-0049 §7) — the teardown a bare SIGKILL would skip.
        boot.handle_store.snapshot_index();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Tests derive chassis mailbox ids by name to address fixture mail —
    // reference id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    use aether_substrate::handle_store::PersistConfig;

    /// Unique scratch dir under the system temp dir, mirroring the
    /// handle-store fast-path tests. PID + millis + a per-call nonce keep
    /// concurrent test processes from colliding.
    fn scratch_dir(tag: &str) -> PathBuf {
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let pid = process::id();
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        let n = NONCE.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("aether-headless-shutdown-{tag}-{pid}-{millis}-{n}"));
        fs::create_dir_all(&path).expect("scratch dir creates");
        path
    }

    /// A pre-set shutdown flag breaks the run loop immediately, so
    /// `run()` returns and the teardown unwinds: `lock.pid` is removed
    /// (ADR-0049 §7 `LockGuard::drop`) and the `index.bin` boot snapshot
    /// is written (issue #1446). The flag is injected directly rather
    /// than via a real signal, so the test never hangs on the tick loop.
    #[test]
    fn shutdown_flag_breaks_loop_and_runs_teardown() {
        let root = scratch_dir("teardown");
        let cfg = PersistConfig {
            root: root.clone(),
            disk_budget_bytes: u64::MAX,
            eviction_tick_secs: 3600,
        };

        let boot = SubstrateBoot::builder("aether-headless-test", env!("CARGO_PKG_VERSION"))
            .persist_config(Some(cfg.clone()))
            .handle_store_max_bytes(Some(64 * 1024 * 1024))
            .build()
            .expect("substrate boot");

        // Boot acquired the on-disk lock (ADR-0049 §7).
        assert!(
            cfg.lock_path().exists(),
            "lock.pid written at boot before teardown"
        );

        let running = Box::new(HeadlessTimerRunning {
            queue: Arc::clone(&boot.queue),
            lifecycle_mailbox: mailbox_id_from_name(
                <LifecycleCapability as Addressable>::NAMESPACE,
            ),
            kind_lifecycle_advance: <LifecycleAdvance as Kind>::ID,
            // Irrelevant — the pre-set flag breaks before the first sleep.
            tick_period: Duration::from_millis(16),
            // Pre-set: the `while !shutdown.load(..)` guard is false on the
            // first check, so the loop body never runs.
            shutdown: Arc::new(AtomicBool::new(true)),
            _boot: boot,
        });

        running.run().expect("run returns on the shutdown flag");

        assert!(
            !cfg.lock_path().exists(),
            "lock.pid removed by LockGuard::drop after run() returns"
        );
        assert!(
            cfg.index_path().exists(),
            "index.bin snapshot written at teardown"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
