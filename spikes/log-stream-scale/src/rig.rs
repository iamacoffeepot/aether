//! The engine under test: one substrate harness with a console, the gatherer,
//! `actors` producers, and the tick driver.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::ActorRef;
use aether_data::Kind;
use aether_harness_substrate::{DEFAULT_TICK_DELTA_MICROS, SubstrateHarness};
use aether_kinds::{CostTailResult, LogTailResult, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_substrate::Subname;
use aether_substrate::mail::log_tap::{TAP_BUFFER, TAP_CLOSED, TAP_MAIL, TAP_MAIL_ROOTED};

use crate::console::Console;
use crate::driver::Driver;
use crate::gatherer::Gatherer;
use crate::kinds::{CONTROL_ATTACH, CONTROL_OPEN, Control};
use crate::producer::Producer;
use crate::stats::{self, GATHER_PENDING, LOAD_DIRECT, LOAD_HANDLERS, LOAD_LINES, LOAD_QUIET, PRODUCED, Padded};

pub struct Args(HashMap<String, String>);

impl Args {
    pub fn parse(raw: impl Iterator<Item = String>) -> Self {
        let pairs = raw.filter_map(|arg| arg.split_once('=').map(|(key, value)| (key.to_owned(), value.to_owned())));
        Self(pairs.collect())
    }

    pub fn text(&self, key: &str, default: &str) -> String {
        self.0.get(key).cloned().unwrap_or_else(|| default.to_owned())
    }

    pub fn number(&self, key: &str, default: u64) -> u64 {
        self.0.get(key).and_then(|value| value.replace('_', "").parse().ok()).unwrap_or(default)
    }
}

/// The knobs every scenario shares.
pub struct Setup {
    pub mode: String,
    pub tap_mode: u8,
    /// `direct` pushes into the ring as the host does for a guest log call;
    /// `tracing` goes through the native `tracing` stack.
    pub logpath: String,
    pub direct: u32,
    pub workers: usize,
    pub actors: usize,
    pub backfill: bool,
    pub buffer_cap: u32,
    pub pending_cap: u32,
    pub batch_cap: u32,
    pub shards: u32,
}

impl Setup {
    pub fn from_args(args: &Args) -> Result<Self, String> {
        let mode = args.text("mode", "off");
        let tap_mode = match mode.as_str() {
            "off" | "nohook" => TAP_CLOSED,
            "mail" => TAP_MAIL,
            "mailroot" => TAP_MAIL_ROOTED,
            "buffer" | "shard" => TAP_BUFFER,
            other => return Err(format!("unknown mode `{other}`")),
        };
        let logpath = args.text("logpath", "direct");
        let pending_cap = u32::try_from(args.number("pending_cap", 4096)).unwrap_or(4096);
        let default_batch = if tap_mode == TAP_BUFFER {
            256
        } else {
            u64::from(pending_cap)
        };
        let default_shards = if mode == "shard" {
            16
        } else {
            1
        };
        Ok(Self {
            shards: u32::try_from(args.number("shards", default_shards)).unwrap_or(1),
            mode: args.text("label", &mode),
            tap_mode,
            direct: u32::from(logpath != "tracing"),
            logpath,
            workers: usize::try_from(args.number("workers", 8)).unwrap_or(8),
            actors: usize::try_from(args.number("actors", 100)).unwrap_or(100),
            backfill: args.number("backfill", 0) != 0,
            buffer_cap: u32::try_from(args.number("buffer_cap", 4096)).unwrap_or(4096),
            pending_cap,
            batch_cap: u32::try_from(args.number("batch_cap", default_batch)).unwrap_or(256),
        })
    }
}

pub struct Rig {
    pub tb: SubstrateHarness,
    pub producers: Vec<ActorRef<Producer>>,
    pub gatherer: ActorRef<Gatherer>,
}

impl Rig {
    pub fn boot(setup: &Setup) -> Result<Self, String> {
        let _ = PRODUCED.set((0..setup.actors).map(|_| Padded::default()).collect());
        LOAD_DIRECT.store(u64::from(setup.direct), Ordering::Relaxed);
        let tb = SubstrateHarness::builder()
            .with_workers(Some(setup.workers))
            .size(16, 16)
            .build()
            .map_err(|error| format!("harness boot: {error:?}"))?;
        let lifecycle = tb.actor_ref::<LifecycleCapability>();

        let console = tb
            .spawn_actor::<Console>(Subname::Named("main"), (), ())
            .finish()
            .map_err(|error| format!("console spawn: {error:?}"))?;
        let gatherer = tb
            .spawn_actor::<Gatherer>(Subname::Named("main"), (console, lifecycle), ())
            .finish()
            .map_err(|error| format!("gatherer spawn: {error:?}"))?;
        let mut producers = Vec::with_capacity(setup.actors);
        for index in 0..setup.actors {
            let producer = tb
                .spawn_actor::<Producer>(Subname::Named(&index.to_string()), index, ())
                .finish()
                .map_err(|error| format!("producer {index} spawn: {error:?}"))?;
            producers.push(producer);
        }
        tb.spawn_actor::<Driver>(Subname::Named("main"), (producers.clone(), lifecycle), ())
            .finish()
            .map_err(|error| format!("driver spawn: {error:?}"))?;

        Ok(Self { tb, producers, gatherer })
    }

    /// Publish the gatherer's handles, and open the tap unless the mode is off.
    pub fn attach(&mut self, setup: &Setup, open: bool) -> Result<(), String> {
        let op = if open && setup.tap_mode != TAP_CLOSED {
            CONTROL_OPEN
        } else {
            CONTROL_ATTACH
        };
        let control = Control {
            op,
            mode: u32::from(setup.tap_mode),
            backfill: u32::from(setup.backfill),
            buffer_cap: setup.buffer_cap,
            pending_cap: setup.pending_cap,
            batch_cap: setup.batch_cap,
            shards: setup.shards,
            reserved: 0,
        };
        self.tb.settle_bytes(self.gatherer, &control).map_err(|error| format!("control: {error:?}"))
    }

    /// Run one frame and return how long it took to settle.
    pub fn frame(&mut self) -> Result<Duration, String> {
        let started = Instant::now();
        self.tb.advance(1, DEFAULT_TICK_DELTA_MICROS).map_err(|error| format!("advance: {error:?}"))?;
        Ok(started.elapsed())
    }

    /// Run one frame, then sleep out the rest of a `pace_hz` period.
    pub fn paced_frame(&mut self, pace_hz: u64) -> Result<Duration, String> {
        let started = Instant::now();
        let took = self.frame()?;
        if pace_hz > 0 {
            let period = Duration::from_nanos(1_000_000_000 / pace_hz);
            if let Some(rest) = period.checked_sub(started.elapsed()) {
                thread::sleep(rest);
            }
        }
        Ok(took)
    }

    /// Stop offering load and run frames until nothing is in flight: the
    /// gatherer's inbox, its pending queue, and the tap buffer are empty and
    /// the subscriber's totals held still for three frames. Returns the
    /// frames it took.
    pub fn drain(&mut self, limit: u32) -> Result<u32, String> {
        set_load(0, 1, 0);
        let mut still = 0;
        let mut last = (0, 0);
        for frame in 0..limit {
            self.frame()?;
            let totals = (stats::get(&stats::CONSOLE_LINES), stats::get(&stats::CONSOLE_SKIPPED));
            let empty = stats::inbox_depth() == 0 && stats::get(&GATHER_PENDING) == 0 && stats::buffer_occupancy() == 0;
            still = if empty && totals == last {
                still + 1
            } else {
                0
            };
            last = totals;
            if still >= 3 {
                return Ok(frame + 1);
            }
        }
        Ok(limit)
    }

    /// The gatherer's per-handler mean execution time from the engine's own
    /// cost table: `(slice handler, tick handler)` in nanoseconds.
    pub fn gatherer_cost(&self) -> (u64, u64) {
        let CostTailResult::Ok { rows } = self.tb.actor_cost(self.gatherer.erase()) else {
            return (0, 0);
        };
        let mean = |kind: u64| rows.iter().find(|row| row.kind_id.0 == kind).map_or(0, |row| row.mean_nanos);
        (mean(LogTailResult::ID.0), mean(Tick::ID.0))
    }
}

pub fn set_load(handlers: u64, lines: u64, quiet: u64) {
    LOAD_HANDLERS.store(handlers, Ordering::Relaxed);
    LOAD_LINES.store(lines, Ordering::Relaxed);
    LOAD_QUIET.store(quiet, Ordering::Relaxed);
}

/// The value at `quantile` of `sorted`, which is ascending.
pub fn quantile(sorted: &[u64], quantile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}
