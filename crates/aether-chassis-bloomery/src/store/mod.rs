//! The `store` native capability (ADR-0149 §The boundary).
//!
//! `SQLite` in WAL mode is the durable single-writer home for the ADR-0149
//! journal: an append-only event log with inbox dedup, a transactional outbox,
//! and the active-membership table whose uniqueness constraint makes bloom
//! sealing all-or-nothing. The capability lives in `aether-chassis-bloomery`
//! rather than a shared cap crate because it is Bloomery-specific and
//! carries the `rusqlite` native dependency (ADR-0149 §Packaging).
//!
//! Identity/runtime split (ADR-0122): the [`StoreCapability`] ZST + the
//! `aether.store.*` kind family are always-on. The full `SQLite`-backed store
//! runtime lives in `runtime.rs` behind the `runtime` feature, while the
//! backend-neutral [`SqliteCorrespondence`] can be selected independently with
//! the narrow `correspondence` feature.
//!
//! A non-Git adapter can persist and reopen opaque object correspondence
//! without linking the Bloomery process or GitHub adapter:
//!
//! ```toml
//! aether-chassis-bloomery = { path = "../aether-chassis-bloomery", default-features = false, features = ["correspondence"] }
//! ```
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//!
//! use aether_bloomery::{BackendObjectId, Digest, SharedCorrespondence};
//! use aether_chassis_bloomery::store::SqliteCorrespondence;
//!
//! let path = "bloomery.sqlite";
//! let digest = Digest::from_bytes([7; 32]);
//! let object = BackendObjectId::new(b"non-git-object".to_vec());
//! let correspondence: SharedCorrespondence = Arc::new(SqliteCorrespondence::open(path)?);
//! correspondence.record(&digest, &object)?;
//! drop(correspondence);
//!
//! let reopened: SharedCorrespondence = Arc::new(SqliteCorrespondence::open(path)?);
//! assert_eq!(reopened.resolve_backend_object(&digest)?, Some(object.clone()));
//! assert_eq!(reopened.resolve_digest(&object)?, Some(digest));
//! # Ok(())
//! # }
//! ```

pub mod kinds;
pub use kinds::*;

// The control-plane transact-mails the wasm control actor drives are defined in
// `aether-bloomery` (cycle avoidance — issue #3497), but they are part of the
// `aether.store.*` surface a store client uses, so re-export them here at the
// same `aether_chassis_bloomery::store` path external consumers already reach.
pub use aether_bloomery::{
    Commit, CommitResult, ConfigRecord, JournalRecord, LoadConfigs, LoadConfigsResult, ReplayJournal,
    ReplayJournalResult,
};

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::{StoreConfig, StoreOverlay};

// Resolving a *bloom's* sealed configuration, distinct from `config` above,
// which is this capability's own boot config (ADR-0090).
#[cfg(feature = "runtime")]
mod resolve;
#[cfg(feature = "runtime")]
pub use resolve::{StoreConfigError, resolve_config};

use aether_actor::actor;

/// Addressing identity for the `aether.store` capability.
#[actor(singleton, root)]
pub struct StoreCapability;

#[cfg(feature = "correspondence")]
mod correspondence;
#[cfg(feature = "correspondence")]
pub use correspondence::SqliteCorrespondence;

#[cfg(feature = "runtime")]
mod adr;
#[cfg(feature = "runtime")]
pub use adr::{AdrBackend, AdrError, AdrView};

#[cfg(feature = "runtime")]
mod commission;
#[cfg(feature = "runtime")]
pub use commission::{
    CancelCommission, CancelCommissionResult, CommissionBackend, CommissionError, CommissionHead, CommissionView,
    CreateCommission, CreateCommissionResult, ListCommissions, ListCommissionsResult, ListedCommission, LoadCommission,
    LoadCommissionResult, RecordCommissionApproval, RecordCommissionApprovalResult, RecordCommissionProjection,
    RecordCommissionProjectionResult, WriteScopeRevision, WriteScopeRevisionResult,
};

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::{
    AppendOutcome, CommitOutcome, JournalWrite, OutstandingOrder, ProofFactRow, ProofFactWrite, RecordOutcome,
    ScopeRunOpen, ScopeRunRow, SealOutcome, SqliteStore, StoreBackend, StoreCapabilityState, StudyRow, now_unix_millis,
};

#[cfg(all(test, feature = "runtime"))]
mod tests;
