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

use std::time::{SystemTime, UNIX_EPOCH};

// ADR-0156 §6 (#3849): the runtime tuning knobs (`AETHER_LOG_FILTER` +
// the three panic-hook knobs) retired the hand-registered `RUNTIME_KNOBS`
// slice — they are now a chassis-declared `RuntimeConfig` derive-`Config`
// member, so they join the composition-derived aggregate (known-keys sweep +
// `--print-config`) like any other member rather than as a residual hand record.

pub(crate) fn now_unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        #[allow(clippy::cast_possible_truncation)]
        let ms = d.as_millis() as u64;
        ms
    })
}
