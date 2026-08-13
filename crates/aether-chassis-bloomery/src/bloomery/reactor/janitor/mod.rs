//! The coordinator janitor: a poll-driven sweeper that reclaims on terminal
//! bloom state, not on boot.
//!
//! Dispatch worktrees, consumed evidence directories, and a bloom's
//! candidate / integration / checkpoint refs are reclaimable facts the journal
//! already records. The happy-path release only runs when a child exits cleanly,
//! so a kill or a crash leaves those artefacts on disk until something
//! reconciles them. This reactor is that something: each tick rebuilds the
//! snapshot from the journal and sweeps whatever the journal says is terminal.
//!
//! It drains no outbox topic — there is no reducer decision to carry out. The
//! identity/runtime split follows ADR-0122.

use aether_actor::actor;

use crate::bloomery::{ExecutorShell, SourceShell};

pub use runtime::{JanitorReactorState, JanitorTick};
pub use sweep::{JanitorPolicy, SweepReport, SweepRequest, sweep};

/// Composer-supplied parts for the janitor reactor.
pub struct JanitorReactorSetup {
    /// The source shell used to prune working refs, or `None` when unconfigured
    /// (disk sweep still runs; ref prune is skipped).
    pub source: Option<SourceShell>,
    /// The executor shell whose local arm answers whether a lane is running.
    pub executor: Option<ExecutorShell>,
    /// The store the journal is replayed from.
    pub store_path: String,
    /// Scratch-worktree base — nonce-keyed checkouts and `*-evidence` dirs live
    /// here.
    pub worktree_base: String,
    /// Per-slot cargo target directory root (`slot-<index>-target`).
    pub target_base: String,
    /// Combined size ceiling across every slot target directory.
    pub lane_target_budget_bytes: u64,
    /// Days to keep consumed evidence of a terminal bloom.
    pub evidence_retention_days: u64,
    /// How often to wake and sweep.
    pub poll_interval_secs: u64,
}

/// Addressing identity for the janitor reactor capability.
#[actor(singleton, root)]
pub struct JanitorReactorCapability;

mod runtime;
mod sweep;

#[cfg(test)]
mod tests;
