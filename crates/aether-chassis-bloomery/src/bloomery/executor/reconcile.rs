//! Boot-time lane reconciliation: what the executor port re-learns about orders
//! that were dispatched by a *previous* coordinator process (issue #4847).
//!
//! Both halves of the executor port keep their in-flight state in process
//! memory — the [`RoutingExecutor`](super::RoutingExecutor)'s nonce→lane map and
//! the [`LocalExecutor`](super::LocalExecutor)'s registry of tracked runs are
//! built empty at construction. The store is not: an order that was dispatched
//! and recorded but never admitted is still sitting in `outstanding_orders` at
//! the next boot, and the reactor already re-tracks it. Until this seam existed
//! the executor did not, so the re-tracked handle resolved against a router that
//! had never heard of the nonce (falling back to the Actions arm) and a local
//! backend with no entry for it (`Unknown` forever, `NoRunForNonce` on cancel) —
//! while the previous process's scratch checkout sat on disk with its `git
//! worktree` registration intact.
//!
//! # Two sources, neither sufficient alone
//!
//! The reconciliation reads both the persisted rows and the on-disk scratch
//! root, because each answers a question the other cannot:
//!
//! - The store is authoritative for **what is still live**. It knows every
//!   outstanding nonce and the exact [`Transformation`] each dispatched — which
//!   is what a re-adopted run needs to bind its returning evidence correctly.
//!   What it cannot say is whether a dispatch ever got as far as materializing
//!   anything locally.
//! - The scratch root is authoritative for **what was materialized locally**. A
//!   dispatch's evidence directory under the local backend's `base_dir` is its
//!   own record that it went to the local lane, surviving independently of any
//!   process memory — and it names the lane slot the dispatch was building in,
//!   which is what a re-adopted run needs in order to hold that slot again
//!   instead of leaving it to be handed out under a live child. What it cannot
//!   say is whether the order that made it is still live: a directory name is a
//!   nonce, not a claim.
//!
//! Intersected, the two answer the question either one leaves open. A live order
//! with a local footprint is a run to re-adopt; a nonce-keyed checkout with no
//! live order is an abandoned one to reclaim; a live order with no footprint has
//! nothing local to reclaim and keeps the Actions fallback, which is where it
//! went.
//!
//! # Re-attachment is by identity, not by pid
//!
//! Re-adopting a run does not reconstruct a [`std::process::Child`]. What a
//! later coordinator can hold is the process identity the previous one
//! recorded beside the dispatch's evidence — pid, process-group id, start
//! time, boot id — and only when that identity still names a live process
//! does [`OrphanedRun`](super::local::OrphanedRun) own the child's group
//! well enough to terminate it (issue #4999). A missing, unreadable, or
//! mismatched record is the unowned run this module started as: cancelling
//! it reports that the child could not be terminated, and the slot it held
//! is quarantined rather than handed to the next dispatch.

use std::collections::BTreeSet;

use aether_bloomery::{ExecutorBackend, Nonce, Transformation};

use super::local::LocalExecutorError;

/// One order the store still holds outstanding, as the reconciliation reads it.
///
/// Carries the whole [`Transformation`] rather than a projection of it because
/// the local backend derives a re-adopted run's evidence-binding subject and
/// lane-specific evidence gates from it by exactly the code its `submit` uses —
/// a projection taken here would be a second spelling of that derivation, free
/// to drift into a run whose synthesized evidence binds the wrong digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutstandingDispatch {
    /// The dispatch's idempotency nonce — the local registry's key and the
    /// scratch directory's name.
    pub nonce: Nonce,
    /// The transformation the order dispatched, decoded from the persisted row.
    pub transformation: Transformation,
}

/// What one reconciliation pass did, for the boot log line and for a caller that
/// needs to know which nonces came back under the local lane.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The nonces re-adopted as local runs — a live order whose scratch
    /// directories the previous process left behind. The router reads this as
    /// the evidence of which lane each nonce went to.
    pub readopted: Vec<Nonce>,
    /// How many abandoned scratch directories were reclaimed — checkouts and
    /// evidence dirs under the local backend's base dir belonging to no live
    /// order.
    pub reclaimed: usize,
}

/// Which lane slots a backend has spoken for right now.
///
/// The janitor's target-directory sweep reads this to decide what it may
/// delete. Slot-precise rather than a single "is anything running" bool,
/// because the two answers differ in the case that matters: a host that always
/// has *some* lane in flight would never enforce its target budget under a
/// blanket bool, while a slot-keyed reading lets an idle slot's directory go
/// and leaves the busy one alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaneOccupancy {
    /// Slot indices held by a tracked run, or by a start that has been given a
    /// slot and has not yet become one. Each is the slot whose checkout and
    /// `CARGO_TARGET_DIR` that dispatch is building in.
    pub slots: BTreeSet<usize>,
    /// A lane child is in flight whose slot this process cannot name — a boot
    /// re-adoption that recovered no slot record (see `readopt`). Nothing
    /// identifies which target directory it is writing into, so while this
    /// holds, no slot directory can be shown to be free.
    pub unattributed: bool,
}

impl LaneOccupancy {
    /// Whether any lane child is in flight at all, attributable or not.
    #[must_use]
    pub fn any_running(&self) -> bool {
        !self.slots.is_empty() || self.unattributed
    }
}

/// The reconciliation face of an executor backend: hand it the orders the store
/// still holds outstanding, get back what it recovered.
///
/// Separate from [`ExecutorBackend`] because it is not part of the port contract
/// — the Actions backend has nothing to reconcile (its run state lives on
/// GitHub's side of the wire and resolves from the nonce alone), so requiring
/// every backend to answer this would be requiring most of them to answer it
/// vacuously.
pub trait ReconcileLanes: Send + Sync {
    /// Reconcile in-process state against `live`, the orders the store still
    /// holds outstanding.
    ///
    /// Infallible by construction: every shortfall a reconciliation can hit — an
    /// unreadable scratch root, a checkout git refuses to release — is a
    /// best-effort cleanup miss that is logged and stepped over, never a reason
    /// to fail a boot that would otherwise run blooms.
    fn reconcile(&self, live: &[OutstandingDispatch]) -> ReconcileReport;

    /// Which lane slots this backend has spoken for right now.
    ///
    /// The janitor will not evict a slot's target directory while that slot is
    /// listed — a cold rebuild is minutes; deleting a live `CARGO_TARGET_DIR`
    /// is a wedged member. Read live, immediately before each removal, because
    /// a slot frees and is re-claimed on the timescale a sweep pass takes.
    /// Default is idle: backends with no local children (Actions) never block
    /// that sweep.
    fn lane_occupancy(&self) -> LaneOccupancy {
        LaneOccupancy::default()
    }
}

/// The local lane as the router fronts it: the executor port plus the
/// reconciliation face, so the router can fan a boot reconciliation out to the
/// arm that owns local run state.
pub trait LocalLane: ExecutorBackend<Error = LocalExecutorError> + ReconcileLanes {}

impl<T: ExecutorBackend<Error = LocalExecutorError> + ReconcileLanes + ?Sized> LocalLane for T {}
