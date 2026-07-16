//! The `store` native capability (ADR-0149 §The boundary).
//!
//! `SQLite` in WAL mode is the durable single-writer home for the ADR-0149
//! journal: an append-only event log with inbox dedup, a transactional outbox,
//! and the active-membership table whose uniqueness constraint makes bloom
//! sealing all-or-nothing. The capability lives in `aether-bloomery-host` (not
//! the shared `aether-capabilities`) because it is Bloomery-specific and
//! carries the `rusqlite` native dependency (ADR-0149 §Packaging).
//!
//! Identity/runtime split (ADR-0122): the [`StoreCapability`] ZST + the
//! `aether.store.*` kind family are always-on; the `SQLite`-backed runtime lives
//! in `runtime.rs` behind the `runtime` feature.

pub mod kinds;
pub use kinds::*;

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::{StoreConfig, StoreOverlay};

use aether_actor::actor;

/// Addressing identity for the `aether.store` capability.
#[actor(singleton)]
pub struct StoreCapability;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::{AppendOutcome, SealOutcome, SqliteStore, StoreBackend, StoreCapabilityState};

#[cfg(all(test, feature = "runtime"))]
mod tests;
