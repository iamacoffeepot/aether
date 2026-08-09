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
//! bare `LocalExecutor` all mount the same way.
//!
//! The connection knobs — token, owner/name, API base — plus the executor-only
//! knobs ride the same ADR-0090 derive-`Config`
//! [`GithubMirrorConfig`] the mirror and source
//! shells use: one GitHub-connection config serves all three caps. When the
//! local model lane is enabled (the default, ADR-0150), [`connect`](ExecutorShell::connect)
//! mounts a `RoutingExecutor` fronting both backends; otherwise the bare Actions
//! backend.
//!
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use aether_bloomery::{EvidenceRef, ExecutionStatus, ExecutorBackend, WorkHandle, WorkOrder};
use aether_bloomery_github::{ActionsExecutor, ExecutorError, GithubError, LaneWorkflows, SharedCorrespondence};

use super::mirror::GithubMirrorConfig;

pub mod local;
mod routing;

pub use local::{
    CaptureIdentity, DEFAULT_LANE_PROGRAM, LaneProgram, LocalExecutor, LocalExecutorError, ProcessTransformRunner,
    RunLifecycle, RunProcess, RunSpec, TransformRunner, mock_lane,
};
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

    fn cancel(&self, handle: &WorkHandle) -> Result<(), Self::Error> {
        Err(ExecutorError::NoRunForNonce(handle.nonce.clone()))
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
        Self { backend: Arc::new(ErrorMapped(backend)) }
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
    #[allow(clippy::redundant_clone)]
    pub fn connect(config: &GithubMirrorConfig) -> Result<Self, GithubError> {
        #[cfg(any(test, feature = "testing"))]
        #[allow(clippy::redundant_clone)]
        if config.uses_fixture() {
            let fake = config.shared_fixture();
            let actions = Arc::new(ActionsExecutor::new(
                fake.clone(),
                Arc::new(fake.clone()) as SharedCorrespondence,
                LaneWorkflows {
                    mechanical: config.executor_workflow_file.clone(),
                    model: config.executor_model_workflow_file.clone(),
                },
                config.executor_dispatch_ref.clone(),
            ));
            if !config.local_lane_enabled {
                return Ok(Self::new(actions));
            }
            #[allow(clippy::redundant_clone)]
            let correspondence: SharedCorrespondence = Arc::new(fake.clone());
            let local = Arc::new(LocalExecutor::from_config(config, correspondence));
            return Ok(Self::new(Arc::new(RoutingExecutor::new(actions, local, config.local_lane_prefixes()))));
        }
        let missing = config.missing_connection_knobs();
        if missing.is_empty() {
            let correspondence = config.connect_correspondence()?;
            // Both backends resolve the order's checkout digest to a real git object
            // through the same persisted correspondence (ADR-0150) — one store over
            // the shared `store_path`, mirroring the source shell.
            let actions = Arc::new(ActionsExecutor::new(
                config.connect_client()?,
                Arc::clone(&correspondence),
                LaneWorkflows {
                    mechanical: config.executor_workflow_file.clone(),
                    model: config.executor_model_workflow_file.clone(),
                },
                config.executor_dispatch_ref.clone(),
            ));

            if !config.local_lane_enabled {
                return Ok(Self::new(actions));
            }
            let local = Arc::new(LocalExecutor::from_config(config, correspondence));
            return Ok(Self::new(Arc::new(RoutingExecutor::new(actions, local, config.local_lane_prefixes()))));
        }

        // Unconfigured. The stub stands in for Actions either way; what the local
        // lane being enabled changes is whether anything routes past it. With it
        // disabled nothing does, and the shell refuses every submit — the reactor
        // reads that combination as a disabled mount and never calls `connect`,
        // but a direct caller still gets the reason rather than a panic.
        let actions = Arc::new(UnconfiguredActionsBackend::new(missing.join(", ")));
        if !config.local_lane_enabled {
            return Ok(Self::new(actions));
        }
        let local = Arc::new(LocalExecutor::from_config(config, config.connect_correspondence()?));
        Ok(Self::new(Arc::new(RoutingExecutor::new(actions, local, config.local_lane_prefixes()))))
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

    /// Cancel the run the handle resolves to.
    ///
    /// # Errors
    /// No run resolves for the nonce, or the cancel surface is unreachable.
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
