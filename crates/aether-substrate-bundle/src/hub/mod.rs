//! Hub chassis (post-issue-763 P5f).
//!
//! The hub is now a thin coordinator:
//!
//! - [`HubChassis`] / [`HubServerDriverCapability`] — Chassis marker +
//!   driver capability. The hub stands up `TraceDispatchCapability` +
//!   `EngineServer` + `RpcServerCapability` and blocks on SIGINT /
//!   SIGTERM. The out-of-process `aether-mcp` crate dials the
//!   `aether.rpc.server` bind.
//!
//! Issue 774 retired the substrate-side `EngineToHub` client residue
//! (`HubClient`, `HubProtocolBackend`, `connect_hub_client`,
//! `dispatch_hub_*`, `loopback_outbound`) along with the wire
//! vocabulary that supported it — the forward-model RPC architecture
//! never used those paths and they were unreachable in practice.

use std::env;
mod chassis;

pub use aether_substrate::Chassis;
pub use chassis::{HubChassis, HubEnv, HubServerDriverCapability, HubServerDriverRunning};

/// Default port the hub binds its `aether.rpc.server` on (issue 763).
/// The hub boots its RPC server unconditionally — it's the target the
/// out-of-process `aether-mcp` coordinator dials (matching that
/// crate's `DEFAULT_HUB_RPC_ADDR`). `AETHER_RPC_PORT` overrides.
pub const DEFAULT_RPC_PORT: u16 = 8901;

/// Parse the `AETHER_RPC_PORT` env var into an optional port number
/// (issue 792). `None` when unset or unparseable. The hub chassis
/// substitutes [`DEFAULT_RPC_PORT`] when this returns `None`; the
/// desktop and headless chassis treat `None` as "don't boot the RPC
/// server" instead.
#[must_use]
// Chassis boot config: the AETHER_RPC_PORT fallback for an absent --rpc-port flag
// (the hub injects this into forked engines), read at the process boundary — not
// a cap config knob.
#[allow(clippy::disallowed_methods)]
pub fn rpc_port_from_env() -> Option<u16> {
    env::var("AETHER_RPC_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
}
