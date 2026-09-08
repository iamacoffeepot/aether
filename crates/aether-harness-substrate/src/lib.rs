//! `SubstrateHarness` — the in-process harness harness (ADR-0067, issue #3765).
//!
//! Two driver modes share [`chassis::SubstrateHarnessChassis`]:
//!
//! - **Binary (`aether-chassis-harness`'s `src/bin/substrate-harness.rs`)**
//!   — runs the chassis events loop on the main thread blocking on
//!   `events_rx.recv()`. Driven by the `aether-mcp` harness through the
//!   forward-model RPC (the substrate hosts `RpcServerCapability`).
//! - **In-process ([`SubstrateHarness`] struct)** — substrate state is owned
//!   by the test thread; mail goes through the same sinks + control
//!   plane but replies route to a `RecordingBackend` loopback instead
//!   of a socket. Rust integration tests link this directly via
//!   `aether_harness_substrate::SubstrateHarness`.
//!
//! The harness boots basics only — trace dispatch, the harness cap,
//! lifecycle, the deterministic synthetic window, the observer mailbox — and
//! each test composes the caps its scenario needs on the builder (issue
//! #3764). GPU capture support plugs in through the [`FrameHook`] hook
//! factory (ADR-0161) from `aether-harness-substrate-capture`, which boots
//! the pumped `aether.render` slot, so this crate never depends on
//! aether-render or wgpu.

pub mod chassis;
mod diagnostics;
mod execute;
mod harness;
#[cfg(test)]
mod mail_latency;
pub mod perf;
mod poll_config;
pub mod pump_stats;
pub mod test_helpers;

pub use chassis::{
    CaptureOutcome, ComposeFn, FrameHook, RenderHookWiring, SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME,
    SubstrateHarnessBuild, SubstrateHarnessChassis, SubstrateHarnessEnv, WORKERS,
};
pub use execute::{
    DEFAULT_POLL_BUDGET, DEFAULT_TICK_DELTA_MICROS, ExecutionError, ExecutionResult, HarnessActor, HarnessOp,
    HarnessOutput, PollObserver,
};
pub use harness::{
    DEFAULT_HEIGHT, DEFAULT_WIDTH, HookFactory, SubstrateHarness, SubstrateHarnessBuilder, SubstrateHarnessError,
};
pub use poll_config::{PollConfig, PollConfigLayer, PollOverlay};
pub use pump_stats::PumpStats;
