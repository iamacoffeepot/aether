//! The executor-port cap shell (ADR-0149 §The boundary / §Execution on Actions,
//! [#3500]) and its host-owned port error.
//!
//! The host mounts an [`ExecutorBackend`] behind a thin shell holding it as an
//! `Arc<dyn ExecutorBackend>`, exactly mirroring the
//! [`SourceShell`](super::SourceShell) / [`ProjectionShell`](super::ProjectionShell)
//! — no backend type crosses into a core module: the shell is the boundary.
//!
//! Since ADR-0149 §The boundary anticipates more than one backend ("the first
//! backend dispatches via `workflow_dispatch`"), the shell's error bound is the
//! host-owned [`ExecutorPortError`] — a union of the Actions
//! ([`ExecutorError`]) and local ([`LocalExecutorError`]) backend faults — not
//! either backend's own error. Any backend whose error converts into
//! `ExecutorPortError` mounts through [`ExecutorShell::new`], which wraps it in a
//! small error-mapping adapter, so the [`RoutingExecutor`]
//! (which already speaks `ExecutorPortError`), a bare `ActionsExecutor`, and a
//! bare `LocalExecutor` all mount the same way. A backend that also answers
//! [`ReconcileLanes`] mounts through [`ExecutorShell::reconciling`] instead,
//! which keeps that face reachable so the reactor can hand the port its
//! outstanding orders at boot (see [`reconcile`]).
//!
//! The connection knobs — token, owner/name, API base — plus the executor-only
//! knobs ride the same ADR-0090 derive-`Config`
//! [`GithubConnectionConfig`] the mirror and source
//! shells use: one GitHub-connection config serves all three caps. When the
//! local model lane is enabled (the default, ADR-0150), [`connect`](ExecutorShell::connect)
//! mounts a `RoutingExecutor` fronting both backends; otherwise the bare Actions
//! backend.
//!
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use aether_bloomery::{EvidenceRef, ExecutionStatus, ExecutorBackend, SharedCorrespondence, WorkHandle, WorkOrder};
use aether_bloomery_github::{ActionsExecutor, ExecutorError, GithubError, LaneWorkflows};

use super::{CoordinatorConfig, GithubConnectionConfig};

pub mod local;
mod reconcile;
mod routing;

pub use local::{
    CaptureIdentity, CapturedObjects, DEFAULT_LANE_PROGRAM, LaneProgram, LocalExecutor, LocalExecutorError,
    OrphanedRun, ProcessTransformRunner, RunLifecycle, RunProcess, RunSpec, TransformRunner, mock_lane,
};
pub use reconcile::{LocalLane, OutstandingDispatch, ReconcileLanes, ReconcileReport};
pub use routing::RoutingExecutor;

/// A backend that always fails with a missing-GitHub-configuration error.
/// Used when the reactor mounts local-only (GitHub unconfigured but
/// `local_lane_enabled`): local lanes still dispatch through the
/// [`LocalExecutor`], while any order that routes to the Actions backend
/// fails fast with a permanent error naming the empty knobs rather than
/// accumulating silently in the outbox.
pub struct UnconfiguredActionsBackend {
    missing: String,
}

impl UnconfiguredActionsBackend {
    /// Build a stub that fails every submit with `missing` in the body.
    #[must_use]
    pub fn new(missing: String) -> Self {
        Self { missing }
    }
}

impl ExecutorBackend for UnconfiguredActionsBackend {
    type Error = ExecutorError;

    fn submit(&self, _order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        Err(ExecutorError::Github(GithubError::Status {
            status: 400,
            body: format!("GitHub not configured: missing {}", self.missing),
        }))
    }

    fn inspect(&self, _handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        Ok(ExecutionStatus::Unknown)
    }

    fn cancel(&self, _handle: &WorkHandle) -> Result<(), Self::Error> {
        // Nothing was ever dispatched through this stub, so there is nothing
        // left running — the "already absent" success ADR-0177's idempotent
        // cancel contract names.
        Ok(())
    }

    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        Err(ExecutorError::NoRunForNonce(handle.nonce.clone()))
    }
}

/// A fault from any executor-port backend the shell fronts — the host-owned union
/// that generalizes the shell's error bound past a single backend's error
/// (ADR-0149 §The boundary anticipates more than one backend). Both backends'
/// errors `From`-convert into it, so a `RoutingExecutor` fronting both can raise
/// either arm through one type.
#[derive(Debug)]
pub enum ExecutorPortError {
    /// The Actions (shared-runner) backend faulted.
    Actions(ExecutorError),
    /// The local-process backend faulted.
    Local(LocalExecutorError),
}

impl fmt::Display for ExecutorPortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Actions(error) => write!(f, "{error}"),
            Self::Local(error) => write!(f, "{error}"),
        }
    }
}

impl Error for ExecutorPortError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Actions(error) => Some(error),
            Self::Local(error) => Some(error),
        }
    }
}

impl From<ExecutorError> for ExecutorPortError {
    fn from(error: ExecutorError) -> Self {
        Self::Actions(error)
    }
}

impl From<LocalExecutorError> for ExecutorPortError {
    fn from(error: LocalExecutorError) -> Self {
        Self::Local(error)
    }
}

/// The executor cap shell: an executor backend behind an `Arc<dyn …>`, so no
/// core module ever names a concrete backend type. Its port error is the
/// host-owned [`ExecutorPortError`].
#[derive(Clone)]
pub struct ExecutorShell {
    backend: Arc<dyn ExecutorBackend<Error = ExecutorPortError> + Send + Sync>,
    // The same backend's reconciliation face, when the mounted one has one.
    // `None` for a bare Actions mount, which has nothing to reconcile: its run
    // state lives on GitHub's side of the wire and resolves from the nonce alone,
    // so a restart costs it nothing.
    reconciler: Option<Arc<dyn ReconcileLanes>>,
    // The local-lane backend the janitor reclaims through. `None` when the
    // local lane is disabled. The same `Arc` the router holds, so occupied-lane
    // counts and on-disk paths agree.
    local: Option<Arc<LocalExecutor>>,
}

// Adapt a backend whose error merely converts into `ExecutorPortError` (a bare
// `ActionsExecutor` raising `ExecutorError`, a bare `LocalExecutor` raising
// `LocalExecutorError`) to the shell's uniform port-error bound. The
// `RoutingExecutor` already raises `ExecutorPortError`, so its adapter is the
// reflexive `From` identity — a no-op map.
struct ErrorMapped<B>(Arc<B>);

impl<B> ExecutorBackend for ErrorMapped<B>
where
    B: ExecutorBackend + Send + Sync,
    ExecutorPortError: From<B::Error>,
{
    type Error = ExecutorPortError;

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        self.0.submit(order).map_err(Into::into)
    }

    fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        self.0.inspect(handle).map_err(Into::into)
    }

    fn cancel(&self, handle: &WorkHandle) -> Result<(), Self::Error> {
        self.0.cancel(handle).map_err(Into::into)
    }

    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        self.0.stream_evidence(handle).map_err(Into::into)
    }
}

impl ExecutorShell {
    /// Mount any executor backend whose error converts into [`ExecutorPortError`]
    /// — a `RoutingExecutor`, a bare `ActionsExecutor`, or a bare `LocalExecutor`
    /// (the demo mounts a fake-backed one, production a routing one).
    #[must_use]
    pub fn new<B>(backend: Arc<B>) -> Self
    where
        B: ExecutorBackend + Send + Sync + 'static,
        ExecutorPortError: From<B::Error>,
    {
        Self { backend: Arc::new(ErrorMapped(backend)), reconciler: None, local: None }
    }

    /// Mount a backend that also answers [`ReconcileLanes`], keeping its
    /// reconciliation face reachable through [`reconcile`](Self::reconcile).
    ///
    /// The production mount, since only the [`RoutingExecutor`] fronts the local
    /// lane whose run state a restart loses.
    #[must_use]
    pub fn reconciling<B>(backend: Arc<B>) -> Self
    where
        B: ExecutorBackend + ReconcileLanes + Send + Sync + 'static,
        ExecutorPortError: From<B::Error>,
    {
        Self { backend: Arc::new(ErrorMapped(Arc::clone(&backend))), reconciler: Some(backend), local: None }
    }

    /// Remember the local-lane backend this shell routes through, so the janitor
    /// reclaims against the same instance the dispatcher occupies.
    #[must_use]
    pub fn with_local(mut self, local: Arc<LocalExecutor>) -> Self {
        self.local = Some(local);
        self
    }

    /// The local-lane backend this shell routes through, when one is mounted.
    #[must_use]
    pub fn local_lane(&self) -> Option<Arc<LocalExecutor>> {
        self.local.clone()
    }

    /// Connect a live executor port from resolved config. When the local model
    /// lane is enabled (the ADR-0150 default), mounts a
    /// [`RoutingExecutor`] fronting both the Actions
    /// (shared-runner verify lanes) and local (model lane) backends; otherwise
    /// the bare Actions backend dispatching the configured wrapper workflow at
    /// the protected pinned ref.
    ///
    /// With GitHub unconfigured the Actions half is an
    /// [`UnconfiguredActionsBackend`] rather than an absent mount (#4626): the
    /// local lane needs no credential, so a bloom whose lanes all route local
    /// still dispatches, and an order that would have gone to Actions is refused
    /// at submit naming the empty knobs.
    ///
    /// # Errors
    /// The underlying `reqwest` client could not be constructed.
    pub fn connect(
        connection: &GithubConnectionConfig,
        coordinator: &CoordinatorConfig,
        correspondence: SharedCorrespondence,
    ) -> Result<Self, GithubError> {
        #[cfg(any(test, feature = "testing"))]
        if connection.uses_fixture() {
            let fake = connection.shared_fixture();
            let actions = Arc::new(ActionsExecutor::new(
                fake,
                Arc::clone(&correspondence),
                LaneWorkflows {
                    mechanical: connection.executor_workflow_file.clone(),
                    model: connection.executor_model_workflow_file.clone(),
                },
                connection.executor_dispatch_ref.clone(),
            ));
            if !coordinator.local_lane_enabled {
                return Ok(Self::new(actions));
            }
            let local = Arc::new(LocalExecutor::from_config(coordinator, correspondence));
            return Ok(Self::reconciling(Arc::new(RoutingExecutor::new(
                actions,
                Arc::clone(&local) as Arc<dyn LocalLane>,
                coordinator.local_lane_prefixes(),
            )))
            .with_local(local));
        }
        let missing = connection.missing_connection_knobs();
        if missing.is_empty() {
            let actions = Arc::new(ActionsExecutor::new(
                connection.connect_client()?,
                Arc::clone(&correspondence),
                LaneWorkflows {
                    mechanical: connection.executor_workflow_file.clone(),
                    model: connection.executor_model_workflow_file.clone(),
                },
                connection.executor_dispatch_ref.clone(),
            ));

            if !coordinator.local_lane_enabled {
                return Ok(Self::new(actions));
            }
            let local = Arc::new(LocalExecutor::from_config(coordinator, correspondence));
            return Ok(Self::reconciling(Arc::new(RoutingExecutor::new(
                actions,
                Arc::clone(&local) as Arc<dyn LocalLane>,
                coordinator.local_lane_prefixes(),
            )))
            .with_local(local));
        }

        // Unconfigured. The stub stands in for Actions either way; what the local
        // lane being enabled changes is whether anything routes past it. With it
        // disabled nothing does, and the shell refuses every submit — the reactor
        // reads that combination as a disabled mount and never calls `connect`,
        // but a direct caller still gets the reason rather than a panic.
        let actions = Arc::new(UnconfiguredActionsBackend::new(missing.join(", ")));
        if !coordinator.local_lane_enabled {
            return Ok(Self::new(actions));
        }
        let local = Arc::new(LocalExecutor::from_config(coordinator, correspondence));
        Ok(Self::reconciling(Arc::new(RoutingExecutor::new(
            actions,
            Arc::clone(&local) as Arc<dyn LocalLane>,
            coordinator.local_lane_prefixes(),
        )))
        .with_local(local))
    }

    /// Reconcile the mounted backend against `live`, the orders the store still
    /// holds outstanding at boot (issue #4847) — re-adopting the runs a previous
    /// coordinator process dispatched and reclaiming the checkouts of orders that
    /// are no longer outstanding.
    ///
    /// A mount with no reconciliation face reports an empty pass rather than an
    /// error: nothing was recovered because there was nothing to recover.
    #[must_use]
    pub fn reconcile(&self, live: &[OutstandingDispatch]) -> ReconcileReport {
        self.reconciler.as_ref().map(|reconciler| reconciler.reconcile(live)).unwrap_or_default()
    }

    /// Submit a fully-resolved work order, returning the nonce-carrying handle.
    ///
    /// # Errors
    /// The dispatch surface is unreachable or refused the dispatch.
    pub fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, ExecutorPortError> {
        self.backend.submit(order)
    }

    /// Inspect the run the handle resolves to.
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean
    /// [`ExecutionStatus::Unknown`] result.
    pub fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, ExecutorPortError> {
        self.backend.inspect(handle)
    }

    /// Cancel the run the handle resolves to. Idempotent (ADR-0177): a run that
    /// is already terminal or already gone is a clean success, so a repeated
    /// cancel of the same expired order never turns retryable into refused.
    ///
    /// # Errors
    /// The cancel surface is unreachable, or the backend faulted — both
    /// retryable. A nonce that resolves no run is not an error.
    pub fn cancel(&self, handle: &WorkHandle) -> Result<(), ExecutorPortError> {
        self.backend.cancel(handle)
    }

    /// Stream the references to the run's uploaded evidence, filtered to the
    /// order's nonce.
    ///
    /// # Errors
    /// No run resolves for the nonce, or the artifact surface is unreachable.
    pub fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, ExecutorPortError> {
        self.backend.stream_evidence(handle)
    }
}
