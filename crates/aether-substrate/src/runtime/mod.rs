//! Process-level invariants set up at boot: the fatal-abort plumbing
//! the wasm-trap abort path reaches for, the `tracing`
//! subscriber installed once per process, and the panic hook that
//! routes panic backtraces through the same logging machinery actor
//! `tracing::*` calls flow through.

pub mod lifecycle;
pub mod log_install;
pub mod panic_hook;
pub mod thread_name;
pub mod trace;

pub use panic_hook::init_panic_hook;

use crate::config::KnobRecord;

use std::time::{SystemTime, UNIX_EPOCH};

/// The runtime tuning knobs registered for config discovery
/// (ADR-0090 §4): the log-filter knob (`log_install::LOG_KNOBS`) and
/// the three panic-hook knobs (`panic_hook::PANIC_KNOBS`), concatenated
/// element-by-element (mirrors `scheduler::SCHEDULER_KNOBS`'s `const`
/// concat shape) so the aggregate stays a `const`. The chassis crates
/// folds this into `chassis_registry()` alongside `SCHEDULER_KNOBS`.
pub const RUNTIME_KNOBS: &[KnobRecord] =
    &[log_install::LOG_KNOBS[0], panic_hook::PANIC_KNOBS[0], panic_hook::PANIC_KNOBS[1], panic_hook::PANIC_KNOBS[2]];

pub(crate) fn now_unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        #[allow(clippy::cast_possible_truncation)]
        let ms = d.as_millis() as u64;
        ms
    })
}
