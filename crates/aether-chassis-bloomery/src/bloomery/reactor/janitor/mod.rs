//! The coordinator janitor: a poll-driven sweeper of working state, and an
//! explicit archive pass for records (ADR-0211).
//!
//! Every artefact the bloomery produces is one of three classes, and each
//! class has one owner:
//!
//! - **Records** — evidence directories and resolved session trees — go to
//!   the archive pass. They are never deleted. The pass is an operator
//!   action, gated on between-blooms, because a move is as disruptive as a
//!   delete to a session that can still resume.
//! - **Working state** — nonce-keyed dispatch checkouts, terminal blooms'
//!   working refs, and the moved-aside leavings of past evictions — belongs
//!   to this sweep. Tree reclaim waits until the coordinator is between
//!   blooms: no active-and-unlanded bloom in the replayed snapshot, and no
//!   outstanding order.
//! - **Caches** — cargo target directories — are the only disk-pressure
//!   kill. Budget eviction runs every tick and evicts only those directories.
//!
//! The 2026-08-25 live-set miss (board-5435; dispatches 3301/3318) reclaimed a
//! walking member's session tree mid-walk; session resumption is protected,
//! so a session checkout lives at least as long as any work that could resume
//! it, and when the work ends the tree is archived as a record rather than
//! deleted.
//!
//! Tree and text reclaim wait until the coordinator is between blooms — no
//! active-and-unlanded bloom in the replayed snapshot, and no outstanding
//! order. Disk pressure evicts slot target directories on every tick; those
//! are regenerable build state, never source trees or text. The 2026-08-25
//! live-set miss (board-5435; dispatches 3301/3318) reclaimed a walking
//! member's session tree mid-walk; session resumption is protected, so a
//! session checkout lives at least as long as any work that could resume it.
//!
//! It drains no outbox topic — there is no reducer decision to carry out. The
//! identity/runtime split follows ADR-0122.

use aether_actor::actor;

use crate::bloomery::{ExecutorShell, SourceShell};

pub use archive::{ArchiveFailure, ArchiveOutcome, ArchiveRequest, ArchiveTier, ArchivedRecord, archive_pass};
pub use kinds::{
    ArchiveFailureView, ArchiveRecords, ArchiveRecordsResult, ArchivedRecordView, ListArchive, ListArchiveResult,
};
pub use runtime::{JanitorReactorState, JanitorTick};
pub use sweep::{JanitorPolicy, SweepReport, SweepRequest, TargetScan, WorkingRefPruner, sweep};

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
    /// Floor between size walks, in seconds. Distinct from
    /// [`Self::poll_interval_secs`], which decides how often the pass runs at
    /// all: the size walk is tens of gigabytes and must not run on the
    /// executor's dispatch cadence. `0` measures on every tick that could
    /// evict.
    pub target_scan_interval_secs: u64,
    /// Days a consumed evidence directory of a terminal bloom must age before
    /// an archive pass will move it.
    pub evidence_retention_days: u64,
    /// Archive-tier root. Empty resolves to `<worktree_base>/archive`.
    pub archive_base: String,
    /// How often to wake and sweep.
    pub poll_interval_secs: u64,
    /// The coordinator repository whose worktrees the janitor lists and
    /// removes — the same path the transform runner materializes against.
    pub repo: String,
}

/// Addressing identity for the janitor reactor capability.
#[actor(singleton, root)]
pub struct JanitorReactorCapability;

mod archive;
mod kinds;
mod records;
mod runtime;
mod sweep;

#[cfg(test)]
mod tests;
