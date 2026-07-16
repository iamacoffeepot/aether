//! The `source` native capability (ADR-0149 §The boundary).
//!
//! The Bloomery source port is native-only: `SourceShell` (`bloomery/source.rs`)
//! wraps the git backend behind an `Arc<dyn SourceBackend>` and exposes
//! `snapshot` / `checkpoint` / `checkpoints` / `integrate` / `land` as plain
//! synchronous methods — it holds the GitHub token and network client, and it
//! carries no mailbox. ADR-0149 bars the wasm guest from touching tokens,
//! shells, or the network, so this capability mounts the shell behind
//! `aether.source.*` transact mail (request + typed reply), mirroring the
//! store port's [`StoreCapability`](crate::store::StoreCapability)
//! (ADR-0122).
//!
//! Identity/runtime split (ADR-0122): the [`SourceCapability`] ZST + the
//! `aether.source.*` kind family are always-on; the `SourceShell`-backed
//! runtime lives in `runtime.rs` behind the `runtime` feature.

pub mod kinds;
pub use kinds::*;

// The seal-claim mail kinds are defined in `aether-bloomery` (like `Commit` /
// `CommitResult`) so the wasm control-core can construct and send them without
// a package cycle; re-exported here so this capability's handlers name them in
// scope, mirroring the store cap's re-export of its control-defined kinds.
pub use aether_bloomery::{ClaimSeal, ClaimSealResult, ReleaseSeal, ReleaseSealResult};

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::{SourceConfig, SourceOverlay};

use aether_actor::actor;

/// Addressing identity for the `aether.source` capability.
#[actor(singleton)]
pub struct SourceCapability;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::SourceCapabilityState;

#[cfg(all(test, feature = "runtime"))]
mod tests;
