//! `aether.rpc` — generic TCP RPC transport (issues 750, 763).
//!
//! The type-erased wire vocabulary (`WireFrame` + its substructs) and
//! the outbound `RpcClient` live in [`wire`] — target-agnostic, always
//! on, re-exported at the module root so `aether_capabilities::rpc::*`
//! resolves them directly (ADR-0124). The substrate-bound
//! [`server::RpcServerCapability`] (the singleton actor that binds a
//! TCP listener, accepts connections, and dispatches inbound `Call`
//! envelopes into the local actor system) sits next to them.
//!
//! See issues 750 and 763 for the full design, ADR-0124 for the layout.

pub mod kinds;
pub mod server;
pub mod wire;

// The cap's own mail vocabulary (`RpcInboundReady`) lives in `kinds`
// (ADR-0121); re-export at the module root so
// `aether_capabilities::rpc::RpcInboundReady` resolves unchanged.
pub use kinds::*;

// Re-export the wire vocabulary + the native `Call` client at the
// module root so `aether_capabilities::rpc::{MailEnvelope, RpcClient,
// WireFrame, ...}` resolves unchanged (ADR-0124). The client
// re-exports are wasm-gated inside `wire`, so the glob carries them
// only on native targets.
pub use wire::*;

pub use server::RpcServerCapability;
#[cfg(not(target_family = "wasm"))]
pub use server::RpcServerConfig;
// `RpcServerHandle` is a live-server boot artifact (published only inside
// runtime `init`), so it rides the runtime half's gate rather than the
// `not(wasm32)` marker gate (ADR-0122). Every consumer is a chassis/test
// build with `runtime` on.
#[cfg(feature = "runtime")]
pub use server::RpcServerHandle;
