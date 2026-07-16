//! The git source-port cap shell ([#3465]).
//!
//! The host mounts the `aether-bloomery-github` [`GitSource`] backend behind a
//! thin shell holding it as an `Arc<dyn SourceBackend>`, exactly mirroring the
//! [`ProjectionShell`](super::ProjectionShell) — no GitHub type crosses into a
//! core module: the shell is the boundary, and only it and the adapter name a
//! github-crate type (ADR-0149 §The boundary, the "no core module names a
//! GitHub type" clause).
//!
//! The connection knobs — token, owner/name, API base, and the CAS-land enable
//! flag — ride the same ADR-0090 derive-`Config`
//! [`GithubMirrorConfig`](super::GithubMirrorConfig) the mirror shell uses:
//! that config already carries `cas_land_enabled` for exactly this port, so one
//! GitHub-connection config serves both caps rather than duplicating the knobs.
//!
//! This slice ships the shell and the demo that drives a synthetic bloom
//! through it (see `tests/source_demo.rs`). Wiring the shell into the chassis
//! boot as an integrate/land-driving capability lands with the migration
//! step 2 executor/review bridge, when the driver that consumes it exists —
//! mirroring the mirror shell's staging.
//!
//! [#3465]: https://github.com/iamacoffeepot/aether/issues/3465

use std::sync::Arc;

use aether_bloomery::{
    BloomId, Checkpoint, ClaimOutcome, Digest, IntegrateOutcome, LandOutcome, SourceBackend, SourceSnapshot,
    WorkpieceId,
};
use aether_bloomery_github::{GitSource, GithubError, ReqwestGithub, SourceError};

use super::mirror::GithubMirrorConfig;

/// The source cap shell: the git source backend behind an `Arc<dyn …>`, so no
/// core module ever names the concrete github-crate type.
#[derive(Clone)]
pub struct SourceShell {
    backend: Arc<dyn SourceBackend<Error = SourceError> + Send + Sync>,
}

impl SourceShell {
    /// Mount an arbitrary source backend — the demo mounts a fake-backed one,
    /// production a `ReqwestGithub`-backed one.
    #[must_use]
    pub fn new(backend: Arc<dyn SourceBackend<Error = SourceError> + Send + Sync>) -> Self {
        Self { backend }
    }

    /// Connect a live GitHub-backed source port from resolved config. The
    /// `cas_land_enabled` knob gates `land` — off by default until ADR-0149
    /// migration step 3.
    ///
    /// # Errors
    /// The underlying `reqwest` client could not be constructed.
    pub fn connect(config: &GithubMirrorConfig) -> Result<Self, GithubError> {
        let client = ReqwestGithub::new(&config.to_github_config())?;
        Ok(Self::new(Arc::new(GitSource::new(client, config.cas_land_enabled))))
    }

    /// Snapshot the source at `base`.
    ///
    /// # Errors
    /// The Git Data surface is unreachable or returned an error status.
    pub fn snapshot(&self, base: &Digest) -> Result<SourceSnapshot, SourceError> {
        self.backend.snapshot(base)
    }

    /// Record an integration checkpoint for `bloom` at `tree`.
    ///
    /// # Errors
    /// The integration branch could not be read or written.
    pub fn checkpoint(&self, bloom: &BloomId, tree: &Digest) -> Result<Checkpoint, SourceError> {
        self.backend.checkpoint(bloom, tree)
    }

    /// Enumerate `bloom`'s recorded checkpoints (for successor reuse).
    ///
    /// # Errors
    /// The integration branch could not be read.
    pub fn checkpoints(&self, bloom: &BloomId) -> Result<Vec<Checkpoint>, SourceError> {
        self.backend.checkpoints(bloom)
    }

    /// Integrate `candidate` onto `bloom`'s integration branch, guarded by the
    /// `expected` checkpoint.
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean conflict /
    /// stale-checkpoint outcomes.
    pub fn integrate(
        &self,
        bloom: &BloomId,
        candidate: &Digest,
        expected: &Checkpoint,
    ) -> Result<IntegrateOutcome, SourceError> {
        self.backend.integrate(bloom, candidate, expected)
    }

    /// Compare-and-swap mainline from `expected_base` to `new_head`.
    ///
    /// # Errors
    /// [`SourceError::LandingDisabled`] while the CAS-land gate is off, or a
    /// transport/backend fault (a moved base is the clean
    /// [`LandOutcome::BaseMoved`], not an error).
    pub fn land(&self, bloom: &BloomId, expected_base: &Digest, new_head: &Digest) -> Result<LandOutcome, SourceError> {
        self.backend.land(bloom, expected_base, new_head)
    }

    /// Acquire the shared seal claim for `bloom` over `workpieces` + the
    /// mainline-admission ref, all-or-nothing (ADR-0150).
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    pub fn claim_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, SourceError> {
        self.backend.claim_seal(bloom, workpieces)
    }

    /// Release the shared seal claim for `bloom` over `workpieces` + the
    /// mainline-admission ref. Idempotent.
    ///
    /// # Errors
    /// A transport or backend fault.
    pub fn release_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<(), SourceError> {
        self.backend.release_seal(bloom, workpieces)
    }
}
