//! `aether.rpc`: a generic TCP RPC transport.
//!
//! The type-erased wire vocabulary (`WireFrame` and its substructs) and the
//! outbound `RpcClient` live in [`wire`], target-agnostic and always on,
//! re-exported at the crate root so `aether_rpc::*` resolves them directly
//! (ADR-0124). [`server::RpcServerCapability`] sits beside them: the singleton
//! actor that binds a TCP listener, accepts connections, and dispatches
//! inbound `Call` envelopes into the local actor system. [`frame_size`] holds
//! the configurable frame ceiling it installs into the codec.
//!
//! Native only. No wasm guest addresses this transport, so the crate carries
//! no identity/runtime feature ladder and its dependencies are flat. It knows
//! nothing of `aether-fleet`: the hub supervisor that forwards over this
//! transport depends on this crate, never the reverse.

// The frame-size config member (ADR-0156 §6) is native-only config machinery:
// its `#[derive(aether_substrate::Config)]` emits a confique layer + clap
// overlay that never build for a wasm guest (which never frames anyway), so the
// module is gated like the `RpcServerConfig` re-export below.
#[cfg(not(target_family = "wasm"))]
pub mod frame_size;
pub mod kinds;
pub mod server;
pub mod wire;

// `FrameSizeOverlay` rides along so the chassis can harvest the wire-frame
// knob's help for its env-only `--help` section (issue 3882); the flag itself
// is deliberately not flattened (the codec knob is pushed set-once at boot).
#[cfg(not(target_family = "wasm"))]
pub use frame_size::{FrameSizeConfig, FrameSizeOverlay};

// The cap's own mail vocabulary (`RpcInboundReady`) lives in `kinds`
// (ADR-0121); re-export at the module root so
// `aether_rpc::RpcInboundReady` resolves unchanged.
pub use kinds::*;

// Re-export the wire vocabulary + the native `Call` client at the
// module root so `aether_rpc::{MailEnvelope, RpcClient,
// WireFrame, ...}` resolves unchanged (ADR-0124). The client
// re-exports are wasm-gated inside `wire`, so the glob carries them
// only on native targets.
pub use wire::*;

pub use server::RpcServerCapability;
#[cfg(not(target_family = "wasm"))]
pub use server::{RpcServerConfig, RpcServerConfigLayer, RpcServerOverlay, RpcServerParams};
// `RpcServerHandle` is a live-server boot artifact (published only inside
// runtime `init`); chassis and test builds read the bound port off it.
pub use server::RpcServerHandle;
