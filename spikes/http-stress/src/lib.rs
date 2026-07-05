//! HTTP stress-test spike shared library.
//!
//! [`handler`] is the trivial native HTTP handler actor (the server + mail
//! round-trip *floor* — no wasm trampoline in the path). [`loadgen`] is the
//! concurrent closed-loop TCP load generator the driver runs against a forked
//! server process.

pub mod handler;
pub mod loadgen;

/// The response body every handler (native and the reused `test.web` wasm
/// fixture) returns, so the two modes move identical bytes and their latency
/// difference is purely the wasm-trampoline cost.
pub const RESPONSE_BODY: &[u8] = b"hello from aether";
