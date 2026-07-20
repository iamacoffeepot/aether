//! Test-bench chassis (ADR-0067).
//!
//! Two driver modes:
//!
//! - **Binary (`src/bin/substrate-bench.rs`)** — runs the chassis events
//!   loop on the main thread blocking on `events_rx.recv()`. Driven
//!   by the `aether-mcp` harness through the forward-model RPC
//!   (the substrate hosts `RpcServerCapability`).
//! - **In-process ([`SubstrateBench`] struct)** — substrate state is owned
//!   by the test thread; mail goes through the same sinks + control
//!   plane but replies route to a `RecordingBackend` loopback instead
//!   of a socket. Rust integration tests (this crate's and sibling
//!   component crates') link this directly via
//!   `aether_substrate_bundle::substrate_bench::SubstrateBench`.

pub mod artifacts;
mod bench;
pub mod cap;
pub mod chassis;
pub mod config;
pub mod events;
mod execute;
#[cfg(test)]
mod mail_latency;
pub mod render;
pub mod test_helpers;
pub mod unsupported_cap;

pub use artifacts::ArtifactGuard;
pub use bench::{DEFAULT_HEIGHT, DEFAULT_WIDTH, SubstrateBench, SubstrateBenchBuilder, SubstrateBenchError};
pub use cap::{SubstrateBenchCapConfig, SubstrateBenchCapability};
pub use chassis::{SubstrateBenchBuild, SubstrateBenchChassis, SubstrateBenchEnv, WORKERS};
pub use config::{RenderSizeConfig, SubstrateBenchClipboardMode};
pub use execute::{BenchOp, BenchOutput, ExecutionError, ExecutionResult};
pub use unsupported_cap::UnsupportedSubstrateBenchCapability;
