//! Counters the benchmark thread reads while the engine runs. Each is written
//! by one actor at a time, so a write is an uncontended store; they are
//! instrumentation, never part of either gathering design.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use aether_substrate::mail::log_tap::LogTap;

#[repr(align(128))]
#[derive(Default)]
pub struct Padded(pub AtomicU64);

/// Lines each producer has logged, by producer index.
pub static PRODUCED: OnceLock<Box<[Padded]>> = OnceLock::new();
/// Set to end every `OP_LOOP` job.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// What the tick driver offers each frame.
pub static LOAD_HANDLERS: AtomicU64 = AtomicU64::new(0);
pub static LOAD_LINES: AtomicU64 = AtomicU64::new(1);
pub static LOAD_QUIET: AtomicU64 = AtomicU64::new(0);
pub static LOAD_DIRECT: AtomicU64 = AtomicU64::new(1);
/// Producers the driver starts looping on its next `Tick`, then zeroes.
pub static LOOP_START: AtomicU64 = AtomicU64::new(0);

/// The engine's tap, published by the gatherer for the sampling thread.
pub static TAP: OnceLock<Arc<LogTap>> = OnceLock::new();

pub static GATHER_SLICES: AtomicU64 = AtomicU64::new(0);
pub static GATHER_LINES_IN: AtomicU64 = AtomicU64::new(0);
pub static GATHER_PENDING: AtomicU64 = AtomicU64::new(0);
pub static GATHER_DROPPED: AtomicU64 = AtomicU64::new(0);
pub static GATHER_CAP_SKIPPED: AtomicU64 = AtomicU64::new(0);
pub static GATHER_RING_LOST: AtomicU64 = AtomicU64::new(0);
pub static GATHER_BATCHES: AtomicU64 = AtomicU64::new(0);
pub static GATHER_LINES_OUT: AtomicU64 = AtomicU64::new(0);
pub static GATHER_TICKS: AtomicU64 = AtomicU64::new(0);
pub static GATHER_TICK_NANOS: AtomicU64 = AtomicU64::new(0);
pub static GATHER_TICK_MAX_NANOS: AtomicU64 = AtomicU64::new(0);

pub static CONSOLE_BATCHES: AtomicU64 = AtomicU64::new(0);
pub static CONSOLE_LINES: AtomicU64 = AtomicU64::new(0);
pub static CONSOLE_FOREIGN: AtomicU64 = AtomicU64::new(0);
pub static CONSOLE_SKIPPED: AtomicU64 = AtomicU64::new(0);
pub static CONSOLE_NANOS: AtomicU64 = AtomicU64::new(0);
pub static CONSOLE_MAX_NANOS: AtomicU64 = AtomicU64::new(0);
pub static CONSOLE_BATCH_SIZES: Mutex<Vec<u32>> = Mutex::new(Vec::new());

pub fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

pub fn add(counter: &AtomicU64, amount: u64) {
    counter.store(counter.load(Ordering::Relaxed) + amount, Ordering::Relaxed);
}

pub fn raise(counter: &AtomicU64, value: u64) {
    if value > counter.load(Ordering::Relaxed) {
        counter.store(value, Ordering::Relaxed);
    }
}

pub fn produced() -> u64 {
    PRODUCED.get().map_or(0, |cells| cells.iter().map(|cell| cell.0.load(Ordering::Relaxed)).sum())
}

/// Slices mailed to the gatherer and not yet handled by it: its inbox depth
/// in the mail design.
pub fn inbox_depth() -> u64 {
    TAP.get().map_or(0, |tap| tap.slices_sent().saturating_sub(get(&GATHER_SLICES)))
}

pub fn buffer_occupancy() -> u64 {
    TAP.get().map_or(0, |tap| tap.counters().occupancy as u64)
}

/// A point-in-time copy of every counter, so a measured window is a difference.
#[derive(Clone, Copy, Default)]
pub struct Snapshot {
    pub produced: u64,
    pub slices: u64,
    pub dropped: u64,
    pub cap_skipped: u64,
    pub ring_lost: u64,
    pub gather_ticks: u64,
    pub gather_tick_nanos: u64,
    pub console_batches: u64,
    pub console_lines: u64,
    pub console_foreign: u64,
    pub console_skipped: u64,
    pub console_nanos: u64,
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        produced: produced(),
        slices: get(&GATHER_SLICES),
        dropped: get(&GATHER_DROPPED),
        cap_skipped: get(&GATHER_CAP_SKIPPED),
        ring_lost: get(&GATHER_RING_LOST),
        gather_ticks: get(&GATHER_TICKS),
        gather_tick_nanos: get(&GATHER_TICK_NANOS),
        console_batches: get(&CONSOLE_BATCHES),
        console_lines: get(&CONSOLE_LINES),
        console_foreign: get(&CONSOLE_FOREIGN),
        console_skipped: get(&CONSOLE_SKIPPED),
        console_nanos: get(&CONSOLE_NANOS),
    }
}
