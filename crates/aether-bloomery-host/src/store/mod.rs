//! The `store` native capability (ADR-0149 §The boundary).
//!
//! `SQLite` in WAL mode is the durable single-writer home for the ADR-0149
//! journal: an append-only event log with inbox dedup, a transactional outbox,
//! and the active-membership table whose uniqueness constraint makes bloom
//! sealing all-or-nothing. The capability lives in `aether-bloomery-host`
//! rather than a shared cap crate because it is Bloomery-specific and
//! carries the `rusqlite` native dependency (ADR-0149 §Packaging).
//!
//! Identity/runtime split (ADR-0122): the [`StoreCapability`] ZST + the
//! `aether.store.*` kind family are always-on; the `SQLite`-backed runtime lives
//! in `runtime.rs` behind the `runtime` feature.

pub mod kinds;
pub use kinds::*;

// The control-plane transact-mails the wasm control actor drives are defined in
// `aether-bloomery` (cycle avoidance — issue #3497), but they are part of the
// `aether.store.*` surface a store client uses, so re-export them here at the
// same `aether_bloomery_host::store` path external consumers already reach.
pub use aether_bloomery::{Commit, CommitResult, JournalRecord, ReplayJournal, ReplayJournalResult};

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::{StoreConfig, StoreOverlay};

use aether_actor::actor;

/// Addressing identity for the `aether.store` capability.
#[actor(singleton)]
pub struct StoreCapability;

#[cfg(feature = "runtime")]
mod correspondence;
#[cfg(feature = "runtime")]
pub use correspondence::SqliteCorrespondence;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::{
    AppendOutcome, CommitOutcome, OutstandingOrder, RecordOutcome, SealOutcome, SqliteStore, StoreBackend,
    StoreCapabilityState, StudyRow,
};

#[cfg(all(test, feature = "runtime"))]
mod tests;
