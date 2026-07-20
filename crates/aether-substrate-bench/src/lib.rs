//! `SubstrateBench` — the in-process bench harness (ADR-0067, issue #3765).
//!
//! Two driver modes share [`chassis::SubstrateBenchChassis`]:
//!
//! - **Binary (`aether-substrate-bundle`'s `src/bin/substrate-bench.rs`)**
//!   — runs the chassis events loop on the main thread blocking on
//!   `events_rx.recv()`. Driven by the `aether-mcp` harness through the
//!   forward-model RPC (the substrate hosts `RpcServerCapability`).
//! - **In-process ([`SubstrateBench`] struct)** — substrate state is owned
//!   by the test thread; mail goes through the same sinks + control
//!   plane but replies route to a `RecordingBackend` loopback instead
//!   of a socket. Rust integration tests link this directly via
//!   `aether_substrate_bench::SubstrateBench`.
//!
//! The bench boots basics only — trace dispatch, the bench cap,
//! lifecycle, the fail-fast headless window, the observer mailbox — and
//! each test composes the caps its scenario needs on the builder (issue
//! #3764). GPU capture support plugs in through the [`FrameHook`] /
//! [`RenderExt`] seam from `aether-substrate-bench-capture`, so this
//! crate never depends on aether-render or wgpu.

mod bench;
pub mod cap;
pub mod chassis;
pub mod events;
mod execute;
#[cfg(test)]
mod mail_latency;
pub mod perf;
mod settlement_config;
pub mod test_helpers;
pub mod unsupported_cap;

pub use bench::{
    DEFAULT_HEIGHT, DEFAULT_WIDTH, HookFactory, SubstrateBench, SubstrateBenchBuilder, SubstrateBenchError,
};
pub use cap::{SubstrateBenchCapConfig, SubstrateBenchCapability};
pub use chassis::{
    BenchWiring, CaptureOutcome, ComposeFn, FrameHook, RenderExt, SubstrateBenchBuild, SubstrateBenchChassis,
    SubstrateBenchEnv, WORKERS,
};
pub use execute::{BenchOp, BenchOutput, ExecutionError, ExecutionResult};
// The derive-emitted `SettlementConfigLayer` rides along for the chassis
// config-dump registry (`chassis_known_keys`), which enumerates every
// knob's `META`.
pub use settlement_config::{SettlementConfig, SettlementConfigLayer};
pub use unsupported_cap::UnsupportedSubstrateBenchCapability;
