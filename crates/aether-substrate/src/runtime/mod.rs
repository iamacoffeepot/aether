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

pub(crate) fn now_unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        #[allow(clippy::cast_possible_truncation)]
        let ms = d.as_millis() as u64;
        ms
    })
}
