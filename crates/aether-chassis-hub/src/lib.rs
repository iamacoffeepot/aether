//! aether-chassis-hub: the hub chassis (ADR-0073, issue #3810;
//! post-issue-763 P5f).
//!
//! The hub is now a thin coordinator:
//!
//! - [`HubChassis`] / [`HubServerDriverCapability`] — Chassis marker +
//!   driver capability. The hub stands up `TraceDispatchCapability` +
//!   `FleetServer` + `RpcServerCapability` and blocks on SIGINT /
//!   SIGTERM. The out-of-process `aether-mcp` crate dials the
//!   `aether.rpc.server` bind.
//!
//! Issue 774 retired the substrate-side `EngineToHub` client residue
//! (`HubClient`, `HubProtocolBackend`, `connect_hub_client`,
//! `dispatch_hub_*`, `loopback_outbound`) along with the wire
//! vocabulary that supported it — the forward-model RPC architecture
//! never used those paths and they were unreachable in practice.

mod chassis;
pub mod cli;
pub mod mcp;

pub use aether_substrate::Chassis;
pub use chassis::{HubChassis, HubServerDriverCapability, HubServerDriverRunning, McpEndpointConfig};
pub use cli::HubCli;
pub use mcp::HubToolProvider;

/// Default port the hub binds its `aether.rpc.server` on (issue 763).
/// The hub boots its RPC server unconditionally — it's the target the
/// out-of-process `aether-mcp` coordinator dials (matching that
/// crate's `DEFAULT_HUB_RPC_ADDR`). `AETHER_RPC_PORT` overrides.
pub const DEFAULT_RPC_PORT: u16 = 8901;

/// Default port the hub binds its Model Context Protocol endpoint on.
///
/// The design's target topology puts the hub's `HttpServerCapability` here
/// and eventually points the tunnel's `/mcp` proxy at it. Until that cutover
/// the port is also the outgoing out-of-process `aether-mcp` child's, so a
/// shadow endpoint enabled while that child is running will fail to bind —
/// loudly, which is the intended way to discover the overlap. The endpoint
/// is off unless `AETHER_MCP_ENABLED` says otherwise, so an ordinary hub
/// never reaches that contention.
pub const DEFAULT_MCP_HTTP_PORT: u16 = 8891;
