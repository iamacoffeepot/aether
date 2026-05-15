//! Hub chassis (post-issue-763 P5f).
//!
//! The hub is now a thin coordinator:
//!
//! - [`HubChassis`] / [`HubServerDriverCapability`] — Chassis marker +
//!   driver capability. The hub stands up `TraceObserverCapability` +
//!   `EngineServer` + `RpcServerCapability` and blocks on SIGINT /
//!   SIGTERM. The out-of-process `aether-mcp` crate dials the
//!   `aether.rpc.server` bind.
//!
//! Issue 774 retired the substrate-side `EngineToHub` client residue
//! (`HubClient`, `HubProtocolBackend`, `connect_hub_client`,
//! `dispatch_hub_*`, `loopback_outbound`) along with the wire
//! vocabulary that supported it — the forward-model RPC architecture
//! never used those paths and they were unreachable in practice.

mod chassis;

pub use aether_substrate::Chassis;
pub use chassis::{HubChassis, HubEnv, HubServerDriverCapability, HubServerDriverRunning};

/// Default port the hub binds its `aether.rpc.server` on (issue 763).
/// The hub boots its RPC server unconditionally — it's the target the
/// out-of-process `aether-mcp` coordinator dials (matching that
/// crate's `DEFAULT_HUB_RPC_ADDR`). `AETHER_RPC_PORT` overrides.
pub const DEFAULT_RPC_PORT: u16 = 8901;
