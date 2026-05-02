//! Test-bench chassis (ADR-0067).
//!
//! Two driver modes:
//!
//! - **Binary (`src/bin/test-bench.rs`)** — connects to a hub via TCP,
//!   runs the chassis events loop on the main thread blocking on
//!   `events_rx.recv()`. Hub-driven exploration (the `spawn_substrate`
//!   MCP path).
//! - **In-process ([`TestBench`] struct)** — no hub, no TCP. Substrate
//!   state is owned by the test thread; mail goes through the same
//!   sinks + control plane but replies route to a loopback channel
//!   instead of a socket. Rust integration tests and `aether-scenario`
//!   link this directly via `aether_substrate_bundle::test_bench::TestBench`.

mod bench;
pub mod chassis;
pub mod events;
pub mod render;

pub use bench::{DEFAULT_HEIGHT, DEFAULT_WIDTH, TestBench, TestBenchBuilder, TestBenchError};
pub use chassis::{
    TestBenchBuild, TestBenchChassis, TestBenchEnv, WORKERS, chassis_control_handler,
};
