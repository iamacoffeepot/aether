//! Process-level invariants set up at boot: the fatal-abort plumbing
//! the cross-class `wait_reply` guard reaches for, the `tracing`
//! subscriber installed once per process, and the panic hook that
//! routes panic backtraces through the same logging machinery actor
//! `tracing::*` calls flow through.

pub mod lifecycle;
pub mod log_install;
pub mod panic_hook;
pub mod trace;

pub use panic_hook::init_panic_hook;
