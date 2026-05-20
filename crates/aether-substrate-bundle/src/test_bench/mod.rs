//! Test-bench chassis (ADR-0067).
//!
//! Two driver modes:
//!
//! - **Binary (`src/bin/test-bench.rs`)** — runs the chassis events
//!   loop on the main thread blocking on `events_rx.recv()`. Driven
//!   by the `aether-mcp` harness through the forward-model RPC
//!   (the substrate hosts `RpcServerCapability`).
//! - **In-process ([`TestBench`] struct)** — substrate state is owned
//!   by the test thread; mail goes through the same sinks + control
//!   plane but replies route to a `RecordingBackend` loopback instead
//!   of a socket. Rust integration tests and `aether-scenario` link
//!   this directly via
//!   `aether_substrate_bundle::test_bench::TestBench`.

mod bench;
pub mod cap;
pub mod chassis;
pub mod events;
mod execute;
pub mod render;
pub mod test_helpers;
pub mod visual;

pub use bench::{DEFAULT_HEIGHT, DEFAULT_WIDTH, TestBench, TestBenchBuilder, TestBenchError};
pub use cap::{TestBenchCapConfig, TestBenchCapability};
pub use chassis::{TestBenchBuild, TestBenchChassis, TestBenchEnv, WORKERS};
pub use execute::{BenchOp, BenchOutput, ExecutionError, ExecutionResult};
