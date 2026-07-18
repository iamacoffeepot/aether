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
    BloomId, Checkpoint, ClaimOutcome, ClaimRefKind, ClaimRefState, Digest, IntegrateOutcome, LandOutcome,
    SourceBackend, SourceSnapshot, WorkpieceId,
};
use aether_bloomery_github::{
    CorrespondenceError, GitObjectId, GitSource, GithubError, SharedCorrespondence, SourceError,
};

use super::mirror::GithubMirrorConfig;

/// The source cap shell: the git source backend behind an `Arc<dyn …>`, so no
/// core module ever names the concrete github-crate type. A live (connected)
/// shell also holds the correspondence handle so it can seed the mainline
/// correspondence (ADR-0150); a fake-backed shell (`new`) seeds the double
/// directly and carries none.
#[derive(Clone)]
pub struct SourceShell {
    backend: Arc<dyn SourceBackend<Error = SourceError> + Send + Sync>,
    correspondence: Option<SharedCorrespondence>,
}

impl SourceShell {
    /// Mount an arbitrary source backend — the demo mounts a fake-backed one,
    /// production a `ReqwestGithub`-backed one. Carries no seedable correspondence
    /// handle (the fake-backed tests seed their double directly); use
    /// [`connect`](Self::connect) for a live shell that can [`seed_mainline`](Self::seed_mainline).
    #[must_use]
    pub fn new(backend: Arc<dyn SourceBackend<Error = SourceError> + Send + Sync>) -> Self {
        Self { backend, correspondence: None }
    }

    /// Connect a live GitHub-backed source port from resolved config, over the
    /// persisted correspondence (ADR-0150). The `cas_land_enabled` knob gates
    /// `land` — on by default since ADR-0149 migration step 3 made the CAS `land`
    /// the landing of record; a `false` knob is the explicit kill switch.
    ///
    /// # Errors
    /// The underlying `reqwest` client or the correspondence store could not be
    /// constructed.
    pub fn connect(config: &GithubMirrorConfig) -> Result<Self, GithubError> {
        let client = config.connect_client()?;
        let correspondence = config.connect_correspondence()?;
        let backend = Arc::new(GitSource::new(client, Arc::clone(&correspondence), config.cas_land_enabled));
        Ok(Self { backend, correspondence: Some(correspondence) })
    }

    /// Seed the mainline correspondence: record the sealed `base` head digest ↔
    /// the repository's real head commit sha `head_commit_sha`, so the first
    /// `snapshot` / `land` has a correspondence to resolve (ADR-0150). The boot
    /// reconcile that reads the live mainline ref supplies the real sha; this is
    /// the entry point it records through.
    ///
    /// # Errors
    /// No correspondence store is mounted (a fake-backed shell), the sha is not a
    /// well-formed git object id, or the store write faulted.
    pub fn seed_mainline(&self, base: &Digest, head_commit_sha: &str) -> Result<(), SourceError> {
        let correspondence = self
            .correspondence
            .as_ref()
            .ok_or_else(|| SourceError::Correspondence(CorrespondenceError::new("no correspondence store mounted")))?;
        let git = GitObjectId::from_hex(head_commit_sha)
            .ok_or_else(|| SourceError::Malformed(format!("mainline head sha `{head_commit_sha}`")))?;
        correspondence.record(base, &git)?;
        Ok(())
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

    /// Acquire `bloom`'s claim refs — one per member workpiece plus the
    /// admission ref — all-or-nothing.
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    pub fn claim_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, SourceError> {
        self.backend.claim_seal(bloom, workpieces)
    }

    /// Transfer the seal from `predecessor` to `successor`: fast-forward the
    /// `carried` refs and the admission ref, fresh-acquire `net_new`, release
    /// `dropped`.
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    pub fn transfer_seal(
        &self,
        predecessor: &BloomId,
        successor: &BloomId,
        carried: &[WorkpieceId],
        net_new: &[WorkpieceId],
        dropped: &[WorkpieceId],
    ) -> Result<ClaimOutcome, SourceError> {
        self.backend.transfer_seal(predecessor, successor, carried, net_new, dropped)
    }

    /// Release `bloom`'s claim refs — the member workpieces plus the admission
    /// ref — each by a CAS to a tombstone.
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    pub fn release_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, SourceError> {
        self.backend.release_seal(bloom, workpieces)
    }

    /// Enumerate every live claim ref, classified by holder — the boot
    /// reconcile's deep-heal detection surface (ADR-0150 amended PR #3556).
    ///
    /// # Errors
    /// The claim-ref namespace could not be read.
    pub fn enumerate_claims(&self) -> Result<Vec<ClaimRefState>, SourceError> {
        self.backend.enumerate_claims()
    }

    /// Idempotent per-ref transfer completion: fast-forward one `ref_kind` from
    /// `predecessor` to `successor` (a ref already at the successor is a no-op).
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    pub fn complete_transfer(
        &self,
        predecessor: &BloomId,
        successor: &BloomId,
        ref_kind: &ClaimRefKind,
    ) -> Result<ClaimOutcome, SourceError> {
        self.backend.complete_transfer(predecessor, successor, ref_kind)
    }

    /// Idempotent per-ref release completion: sweep a tombstoned ref (`None`
    /// holder) or release a ref held by `Some(bloom)`.
    ///
    /// # Errors
    /// A transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    pub fn complete_release(
        &self,
        expected_holder: Option<&BloomId>,
        ref_kind: &ClaimRefKind,
    ) -> Result<ClaimOutcome, SourceError> {
        self.backend.complete_release(expected_holder, ref_kind)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery_github::testing::FakeGithub;
    use aether_bloomery_github::{GitSource, SharedCorrespondence};

    use super::{Arc, Digest, SourceShell};

    #[test]
    fn seed_mainline_records_a_resolvable_base_correspondence() {
        // Tripwire: seed_mainline records the sealed base digest ↔ the repo's real
        // (40-hex sha1) mainline head commit, so a later `resolve_git` reads it back
        // — the record path the deferred boot reconcile drives, verified here
        // without the live mainline read that supplies the sha.
        let fake = FakeGithub::new();
        let correspondence: SharedCorrespondence = Arc::new(fake.clone());
        let backend = Arc::new(GitSource::new(fake, Arc::clone(&correspondence), true));
        let shell = SourceShell { backend, correspondence: Some(Arc::clone(&correspondence)) };

        let base = Digest::from_bytes([7; 32]);
        let head = "3a3f8c0b9e1d2a4f6b8c0e2d4a6f8b0c1e3d5a7f";
        shell.seed_mainline(&base, head).unwrap();

        let resolved =
            correspondence.resolve_git(&base).unwrap().expect("the seeded base resolves to the mainline head");
        assert_eq!(resolved.to_hex(), head, "the resolved object is the real 40-hex sha1, not a hex-punned digest");
    }

    #[test]
    fn seed_mainline_without_a_mounted_correspondence_errors() {
        // Tripwire: a fake-backed shell (`new`) carries no correspondence store, so
        // seed_mainline refuses cleanly rather than silently no-op-ing the record.
        let fake = FakeGithub::new();
        let shell = SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake), true)));
        assert!(shell.seed_mainline(&Digest::from_bytes([7; 32]), "3a3f8c0b9e1d2a4f6b8c0e2d4a6f8b0c1e3d5a7f").is_err());
    }
}
