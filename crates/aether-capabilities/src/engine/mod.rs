//! `aether.engine` — engine-management capability cluster (issue 763).
//!
//! - [`proxy::EngineProxy`] (P3) — the per-engine proxy actor that
//!   wraps one outbound RPC connection to a substrate; the bridge core
//!   of the forward-model architecture.
//! - [`server::EngineServer`] (P4) — the engines cap (`list` / `spawn`
//!   / `terminate`) that supervises a fleet of proxies, fork+execing
//!   substrates and connecting a proxy to each.
//!
//! See issue 763 for the full design.

pub mod kinds;
pub mod proxy;
pub mod server;
#[cfg(feature = "native")]
mod store;

pub use kinds::{
    CallSettled, EngineAlive, EngineDied, EngineHeartbeatTick, ForwardEnvelope, RouteEnvelope,
};
pub use proxy::EngineProxy;
#[cfg(not(target_family = "wasm"))]
pub use proxy::EngineProxyConfig;
pub use server::EngineServer;
#[cfg(not(target_family = "wasm"))]
pub use server::{EngineConfig, EngineConfigLayer, EngineOverlay};
