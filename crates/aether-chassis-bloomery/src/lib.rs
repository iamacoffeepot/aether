//! aether-chassis-bloomery: the bloomery chassis (issue #6244), the
//! journal-driven engine. Boots the shared base stratum plus the component
//! host, HTTP egress, the workspace actor, and the RPC server, mounts the journal owner and the
//! bundle driver over one journal root, and only then binds the RPC listener
//! (issue #6399). Produces the `aether-bloomery` binary over the shared
//! `aether-chassis` composition layer.
//!
//! HTTP egress is one of the engine's two integrations (ADR-0234 decision 7):
//! Sampled programs fetch through it, and it keeps the capability's own
//! deny-by-default allowlist, so a fetch reaches only the hosts an operator
//! names with `--http-allowlist` and any other fetch is recorded as a refusal.
//! The other is the `aether.workspace` actor (ADR-0237 decision 8), which
//! imports digest-pinned images into the journal through the Docker Engine
//! API at `--workspace-endpoint`, writing only through the store of the
//! journal this engine opened.
//!
//! The composition answers ADR-0226's deferred "chassis mounting": the driver
//! and the journal become RPC-addressable mailboxes on this engine, so journal
//! writes are reachable from any local process that can reach a bound RPC
//! port. The port binds only once both are mounted, so reachable means
//! ready. The RPC server rides every substrate chassis (ADR-0155 §3); argv
//! stays the machine channel (ADR-0162), so a flag this engine does not
//! understand —
//! `--boot-manifest` — fails at clap parse rather than booting half-configured.

#![forbid(unsafe_code)]

pub mod chassis;
pub mod cli;
pub mod config;
mod mount;

pub use chassis::BloomeryChassis;
pub use cli::BloomeryCli;
pub use config::BloomeryConfig;
pub use mount::Mounted;
