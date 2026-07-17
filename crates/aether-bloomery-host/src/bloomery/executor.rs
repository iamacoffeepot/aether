//! The Actions executor-port cap shell (ADR-0149 §The boundary / §Execution on
//! Actions, [#3500]).
//!
//! The host mounts the `aether-bloomery-github` [`ActionsExecutor`] backend
//! behind a thin shell holding it as an `Arc<dyn ExecutorBackend>`, exactly
//! mirroring the [`SourceShell`](super::SourceShell) /
//! [`ProjectionShell`](super::ProjectionShell) — no GitHub type crosses into a
//! core module: the shell is the boundary, and only it and the adapter name a
//! github-crate type (ADR-0149 §The boundary, the "no core module names a
//! GitHub type" clause).
//!
//! The connection knobs — token, owner/name, API base — plus the two
//! executor-only knobs (the wrapper `executor_workflow_file` and the protected
//! `executor_dispatch_ref`) ride the same ADR-0090 derive-`Config`
//! [`GithubMirrorConfig`](super::GithubMirrorConfig) the mirror and source
//! shells use: one GitHub-connection config serves all three caps rather than
//! duplicating the knobs.
//!
//! This slice ships the shell and the demo that drives a synthetic work order
//! through it against the adapter's `FakeGithub` (see `tests/executor_demo.rs`).
//! Wiring the shell into the chassis boot as an outbox-driven dispatch
//! capability lands with the reducer runtime ([#3497]) that produces dispatch
//! decisions to drive it — mirroring how the mirror and source shells shipped
//! shell-first.
//!
//! [#3497]: https://github.com/iamacoffeepot/aether/issues/3497
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500

use std::sync::Arc;

use aether_bloomery::{EvidenceRef, ExecutionStatus, ExecutorBackend, WorkHandle, WorkOrder};
use aether_bloomery_github::{ActionsExecutor, ExecutorError, GithubError};

use super::mirror::GithubMirrorConfig;

/// The executor cap shell: the Actions executor backend behind an `Arc<dyn …>`,
/// so no core module ever names the concrete github-crate type.
#[derive(Clone)]
pub struct ExecutorShell {
    backend: Arc<dyn ExecutorBackend<Error = ExecutorError> + Send + Sync>,
}

impl ExecutorShell {
    /// Mount an arbitrary executor backend — the demo mounts a fake-backed one,
    /// production a `ReqwestGithub`-backed one.
    #[must_use]
    pub fn new(backend: Arc<dyn ExecutorBackend<Error = ExecutorError> + Send + Sync>) -> Self {
        Self { backend }
    }

    /// Connect a live GitHub-backed executor port from resolved config,
    /// dispatching the configured wrapper workflow at the protected pinned ref.
    ///
    /// # Errors
    /// The underlying `reqwest` client could not be constructed.
    pub fn connect(config: &GithubMirrorConfig) -> Result<Self, GithubError> {
        let client = config.connect_client()?;
        let backend =
            ActionsExecutor::new(client, config.executor_workflow_file.clone(), config.executor_dispatch_ref.clone());
        Ok(Self::new(Arc::new(backend)))
    }

    /// Submit a fully-resolved work order, returning the nonce-carrying handle.
    ///
    /// # Errors
    /// The dispatch surface is unreachable or refused the dispatch.
    pub fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, ExecutorError> {
        self.backend.submit(order)
    }

    /// Inspect the run the handle resolves to.
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean
    /// [`ExecutionStatus::Unknown`] result.
    pub fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, ExecutorError> {
        self.backend.inspect(handle)
    }

    /// Cancel the run the handle resolves to.
    ///
    /// # Errors
    /// No run resolves for the nonce, or the cancel surface is unreachable.
    pub fn cancel(&self, handle: &WorkHandle) -> Result<(), ExecutorError> {
        self.backend.cancel(handle)
    }

    /// Stream the references to the run's uploaded evidence, filtered to the
    /// order's nonce.
    ///
    /// # Errors
    /// No run resolves for the nonce, or the artifact surface is unreachable.
    pub fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, ExecutorError> {
        self.backend.stream_evidence(handle)
    }
}
