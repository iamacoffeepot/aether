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

use aether_bloomery::{EvidenceRef, ExecutionStatus, ExecutorBackend, WorkHandle, WorkOrder};
use aether_bloomery_github::ExecutorError;

use super::executor::ExecutorPortError;
use super::local_executor::LocalExecutorError;

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
    local: Arc<dyn ExecutorBackend<Error = LocalExecutorError> + Send + Sync>,
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
        local: Arc<dyn ExecutorBackend<Error = LocalExecutorError> + Send + Sync>,
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
    // never submitted (the dispatch reactor always submits before it inspects, so
    // a miss is only the fallback path, never the normal one).
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
        match self.lane_of(&handle.nonce.0) {
            Lane::Actions => self.actions.cancel(handle)?,
            Lane::Local => self.local.cancel(handle)?,
        }
        // A cancel is terminal — drop the routing record so `routed` tracks
        // in-flight orders, not the lifetime total.
        self.lock().remove(&handle.nonce.0);
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
