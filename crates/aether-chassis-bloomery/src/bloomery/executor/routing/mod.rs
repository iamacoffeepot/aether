//! The config-driven lane→backend router (ADR-0149 §The boundary, ADR-0150 —
//! issue #3586).
//!
//! One [`ExecutorBackend`] fronting both the Actions (shared-runner) and local
//! (operator-machine) backends, selecting per order by the typed
//! [`Transformation::command`](aether_bloomery::Transformation) id. The default
//! routes the model-driven `construct.*` lanes to the local backend (ambient
//! `claude` auth, ADR-0150) and everything else — the computationally-heavy
//! mechanical verify lanes — to the zero-secret Actions backend, and the prefix
//! set is config so any lane can be flipped to local as a release valve (Actions
//! outage, quota, offline work).
//!
//! # Resolving a handle's lane
//!
//! `submit` picks the lane from the order's command; the other three messages
//! carry only the nonce handle, not the command, so the router records which lane
//! each submitted nonce went to and re-resolves `inspect` / `cancel` /
//! `stream_evidence` against that record. A nonce never submitted through this
//! router (which the dispatch reactor never produces — it submits before it
//! inspects) falls back to the Actions arm.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aether_bloomery::{
    BackendId, EvidenceRef, ExecutionStatus, ExecutorBackend, ObservedLaneWrites, WorkHandle, WorkOrder,
};
use aether_bloomery_github::ExecutorError;

use super::reconcile::{LaneOccupancy, LocalLane, OutstandingDispatch, ReconcileLanes, ReconcileReport};
use super::{ACTIONS_BACKEND, ExecutorPortError, LOCAL_BACKEND};

/// Which backend an order routed to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lane {
    /// The zero-secret Actions (shared-runner) backend.
    Actions,
    /// The local-process backend (ambient credential, ADR-0150).
    Local,
}

/// The lane→backend router. Holds both backends behind the port and the resolved
/// local-lane prefix set; raises the host-owned [`ExecutorPortError`] so either
/// arm's fault surfaces through one type.
pub struct RoutingExecutor {
    actions: Arc<dyn ExecutorBackend<Error = ExecutorError> + Send + Sync>,
    local: Arc<dyn LocalLane>,
    local_prefixes: Vec<String>,
    routed: Mutex<HashMap<String, Lane>>,
}

impl RoutingExecutor {
    /// Build a router over both backends, routing any command whose id starts
    /// with one of `local_prefixes` to the local backend and everything else to
    /// Actions.
    #[must_use]
    pub fn new(
        actions: Arc<dyn ExecutorBackend<Error = ExecutorError> + Send + Sync>,
        local: Arc<dyn LocalLane>,
        local_prefixes: Vec<String>,
    ) -> Self {
        Self { actions, local, local_prefixes, routed: Mutex::new(HashMap::new()) }
    }

    fn lane_for_command(&self, command: &str) -> Lane {
        if self.local_prefixes.iter().any(|prefix| command.starts_with(prefix.as_str())) {
            Lane::Local
        } else {
            Lane::Actions
        }
    }

    // The lane a submitted nonce routed to, or Actions for a nonce this router
    // never submitted and boot reconciliation did not recover (the dispatch reactor
    // always submits before it inspects, so a miss is only the fallback path, never
    // the normal one).
    fn lane_of(&self, nonce: &str) -> Lane {
        self.lock().get(nonce).copied().unwrap_or(Lane::Actions)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Lane>> {
        self.routed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ExecutorBackend for RoutingExecutor {
    type Error = ExecutorPortError;

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        let lane = self.lane_for_command(&order.transformation.command);
        let handle = match lane {
            Lane::Actions => self.actions.submit(order)?,
            Lane::Local => self.local.submit(order)?,
        };
        self.lock().insert(order.nonce.0.clone(), lane);
        Ok(handle)
    }

    fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        Ok(match self.lane_of(&handle.nonce.0) {
            Lane::Actions => self.actions.inspect(handle)?,
            Lane::Local => self.local.inspect(handle)?,
        })
    }

    fn cancel(&self, handle: &WorkHandle) -> Result<(), Self::Error> {
        // The routing record is deliberately *kept* across a cancel (ADR-0177).
        // `cancel` is idempotent, which means a second one has to reach the same
        // backend as the first — and the deadline enforcement reissues it on
        // every tick until the expired order is admitted. Dropping the record
        // here would send the repeat to the Actions fallback, spending a GitHub
        // round trip probing both wrappers for a nonce that only ever existed on
        // the local lane. The retained entry is one small row per cancelled
        // order, which timeouts produce rarely by construction.
        match self.lane_of(&handle.nonce.0) {
            Lane::Actions => self.actions.cancel(handle)?,
            Lane::Local => self.local.cancel(handle)?,
        }
        Ok(())
    }

    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        let refs = match self.lane_of(&handle.nonce.0) {
            Lane::Actions => self.actions.stream_evidence(handle)?,
            Lane::Local => self.local.stream_evidence(handle)?,
        };
        // `stream_evidence` is the last message the intake cycle sends for a
        // completed order (`run_intake_cycle` inspects, then streams, then the
        // reactor prunes the handle), so evict the routing record here to bound
        // `routed`. Eviction cannot ride `inspect` instead: the cycle streams the
        // same nonce immediately after a `Completed` inspect, and dropping the
        // record there would misroute that stream to the Actions fallback.
        self.lock().remove(&handle.nonce.0);
        Ok(refs)
    }

    /// Only the local arm has working trees this process can read (ADR-0204).
    /// An Actions run's checkout lives on GitHub's side of the wire, so the
    /// router does not ask it.
    fn observe_writes(&self) -> Vec<ObservedLaneWrites> {
        self.local.observe_writes()
    }

    /// The arm `inspect` / `cancel` / `stream_evidence` would resolve this
    /// handle against, read from the same routing record they read (#5412), so
    /// a caller grouping handles by arm groups them by where they will actually
    /// go — fallback included.
    fn backend_for(&self, handle: &WorkHandle) -> BackendId {
        match self.lane_of(&handle.nonce.0) {
            Lane::Actions => ACTIONS_BACKEND,
            Lane::Local => LOCAL_BACKEND,
        }
    }
}

impl ReconcileLanes for RoutingExecutor {
    /// Rebuild the routing map for orders a previous process dispatched (issue
    /// #4847), from what the local arm can still see of them.
    ///
    /// The routing record is process memory, so after a restart every outstanding
    /// nonce misses and takes the Actions fallback — a local-lane order's cancel
    /// is routed to GitHub, probes both run wrappers, and returns `Ok` without
    /// ever reaching the arm that holds the run. Seeding the map closes that.
    ///
    /// The lane is read from the **local arm's observed footprint** rather than
    /// re-derived by running the persisted command back through the prefix set
    /// `submit` selects with. Re-derivation looks equivalent
    /// and is not: the prefix set is config, so an operator who flips a lane
    /// between restarts — the release valve the prefixes exist for — would have
    /// every order dispatched under the old setting re-routed to the arm it never
    /// went to, which is precisely the orders the flip touched. The scratch
    /// directory is the dispatch's own record of where it went, and it does not
    /// move when config does.
    ///
    /// An order the local arm did not recover keeps the Actions fallback, which is
    /// where it went if it left no local footprint at all. Existing records are
    /// never overwritten: a nonce this process submitted itself already carries the
    /// lane it actually used.
    fn reconcile(&self, live: &[OutstandingDispatch]) -> ReconcileReport {
        let report = self.local.reconcile(live);

        let mut routed = self.lock();
        for nonce in &report.readopted {
            routed.entry(nonce.0.clone()).or_insert(Lane::Local);
        }
        drop(routed);

        report
    }

    fn lane_occupancy(&self) -> LaneOccupancy {
        self.local.lane_occupancy()
    }

    fn started_nonces(&self) -> Vec<String> {
        self.local.started_nonces()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
