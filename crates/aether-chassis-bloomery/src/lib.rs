//! aether-chassis-bloomery: the bloomery chassis (issue #6244), the
//! journal-driven engine. Boots the shared base stratum plus the component
//! host and the RPC server, mounts the journal owner and the bundle driver
//! over one journal file, and only then binds the RPC listener (issue
//! #6399). Produces the `aether-bloomery` binary over the shared
//! `aether-chassis` composition layer.
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
mod driver;
mod mount;

pub use chassis::BloomeryChassis;
pub use cli::BloomeryCli;
pub use config::BloomeryConfig;
pub use driver::{BloomeryDriverCapability, BloomeryDriverRunning};
pub use mount::Mounted;
