//! The coordinator janitor (resource reclamation on terminal state).
//!
//! A poll-driven reactor that reconciles from the journal rather than draining
//! an outbox topic: when a bloom lands or is superseded, its leftover dispatch
//! worktrees, consumed evidence (after the configured retention window), and
//! candidate / integration / checkpoint refs are reclaimable facts the journal
//! already records. Kills and crashes are the cases the happy-path release
//! misses; this sweeper does not care how a run ended.
//!
//! The identity/runtime split follows ADR-0122 — this ZST is the addressing
//! identity; the state-bearing logic is [`runtime`].

use std::sync::Arc;

use aether_actor::actor;

use crate::bloomery::{LocalExecutor, SourceShell};

pub use runtime::{JanitorReactorState, JanitorTick};

/// Composer-supplied parts for the janitor reactor.
pub struct JanitorReactorSetup {
    /// The local-lane backend whose scratch, evidence, and target dirs this
    /// reactor reclaims. `None` when the local lane is disabled — ref pruning
    /// still runs against the journal and the source.
    pub local: Option<Arc<LocalExecutor>>,
    /// The connected source shell, or `None` when unconfigured (no remote refs
    /// to prune).
    pub source: Option<SourceShell>,
    /// The store the journal is replayed from.
    pub store_path: String,
    /// How often to wake and sweep.
    pub poll_interval_secs: u64,
    /// Combined ceiling across every per-slot cargo target directory, in bytes.
    /// `0` disables the budget sweep.
    pub lane_target_budget_bytes: u64,
    /// Days to keep consumed evidence after its bloom is terminal. `0` reclaims
    /// on the next sweep. Live blooms' evidence is never deleted.
    pub evidence_retain_days: u64,
    /// Root of the per-run throwaway build trees. Empty skips the separate
    /// scratch sweep (those trees then live under evidence dirs).
    pub lane_scratch: String,
}

/// Addressing identity for the janitor reactor capability.
#[actor(singleton, root)]
pub struct JanitorReactorCapability;

mod runtime;

#[cfg(test)]
mod tests;
