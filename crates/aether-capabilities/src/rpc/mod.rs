//! `aether.rpc` — generic TCP RPC transport (issues 750, 763).
//!
//! The type-erased wire vocabulary (`WireFrame` + its substructs) and
//! the outbound `RpcClient` live in `aether-rpc` (ADR-0102) — a crate
//! with no path to `aether-substrate`. This module re-exports them at
//! their original `aether_capabilities::rpc::*` paths and keeps the
//! substrate-bound [`server::RpcServerCapability`] (the singleton actor
//! that binds a TCP listener, accepts connections, and dispatches
//! inbound `Call` envelopes into the local actor system) next to them.
//!
//! See issues 750 and 763 for the full design, ADR-0102 for the split.

pub mod kinds;
pub mod server;

// The cap's own mail vocabulary (`RpcInboundReady`) lives in `kinds`
// (ADR-0121); re-export at the module root so
// `aether_capabilities::rpc::RpcInboundReady` resolves unchanged.
pub use kinds::*;

// Shared round-trip test scaffolding (echo actor + its kinds), used by
// the `server` test modules and the `engine::proxy` test — `pub(crate)`
// so cross-module test code outside `rpc` can reach it.
#[cfg(test)]
pub(crate) mod test_echo;

// Re-export the wire vocabulary + the native `Call` client from
// `aether-rpc` so `aether_capabilities::rpc::{MailEnvelope, RpcClient,
// WireFrame, ...}` keeps resolving unchanged (ADR-0102). The client
// re-exports are themselves `wasm32`-gated inside `aether-rpc`, so the
// glob carries them only on native targets.
pub use aether_rpc::rpc::*;

pub use server::RpcServerCapability;
#[cfg(not(target_arch = "wasm32"))]
pub use server::RpcServerConfig;
// `RpcServerHandle` is a live-server boot artifact (published only inside
// runtime `init`), so it rides the runtime half's gate rather than the
// `not(wasm32)` marker gate (ADR-0122). Every consumer is a chassis/test
// build with `runtime` on.
#[cfg(feature = "runtime")]
pub use server::RpcServerHandle;
