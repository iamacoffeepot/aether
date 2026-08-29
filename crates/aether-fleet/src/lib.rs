//! `aether.fleet` — engine-management capability cluster (issue 763).
//!
//! - [`FleetProxy`] (P3) — the per-engine proxy actor that
//!   wraps one outbound RPC connection to a substrate; the bridge core
//!   of the forward-model architecture.
//! - [`FleetServer`] (P4) — the engines cap (`list` / `spawn`
//!   / `terminate`) that supervises a fleet of proxies, fork+execing
//!   substrates and connecting a proxy to each.
//!
//! Native-only — the cap fork+execs substrate processes — so the crate carries
//! no ADR-0122 identity/runtime marker ladder and its dependencies are flat and
//! unconditional. The RPC transport it forwards over lives in `aether-rpc`; the
//! dependency runs strictly this way, never back.
//!
//! See issue 763 for the full design.

pub mod child_env;
pub mod kinds;
mod proxy;
mod server;
mod store;

pub use proxy::FleetProxy;
#[cfg(not(target_family = "wasm"))]
pub use proxy::FleetProxyConfig;
pub use server::FleetServer;
#[cfg(not(target_family = "wasm"))]
// `RestartPolicy` rides along because it is the return type of the public
// `FleetConfig::restart_policy()` — a caller that reads the resolved
// supervision policy needs to be able to name it.
#[cfg(not(target_family = "wasm"))]
pub use server::{FleetConfig, FleetConfigLayer, FleetOverlay, RestartPolicy};
