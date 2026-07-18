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
    BloomId, Checkpoint, ClaimOutcome, ClaimRefKind, ClaimRefState, Digest, IntegrateOutcome, IntegrationPosition,
    LandOutcome, Snapshot, SourceBackend, SourceSnapshot, WorkpieceId,
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

    /// Establish the initial mainline-base correspondence at boot: read the
    /// repository's live mainline head sha and seed
    /// [`Snapshot::GENESIS_MAINLINE`] ↔ that sha (issue #3615). Run once, after
    /// [`connect`](Self::connect) (which opens no network) and before the first
    /// snapshot, mirroring the claim/deep-heal boot reconcile surface. The
    /// control core starts every snapshot at `Snapshot::GENESIS_MAINLINE` and
    /// blooms seal against it, so seeding that exact digest to the real head is
    /// the authoritative initial correspondence: the first land's reverse-resolve
    /// of the real mainline object returns the genesis base and the CAS proceeds
    /// instead of faulting `UnresolvedCorrespondence`. Seeding
    /// `base ↔ current-head` lazily on the first snapshot would instead equate
    /// base and head and defeat `land`'s base-moved detection, so the genesis
    /// read is the authoritative seam.
    ///
    /// # Errors
    /// The mainline ref is unreachable, no correspondence store is mounted (a
    /// fake-backed shell), the head sha is malformed, or the store write faulted.
    pub fn reconcile_genesis_mainline(&self) -> Result<(), SourceError> {
        let head_sha = self.backend.mainline_head_sha()?;
        self.seed_mainline(&Snapshot::GENESIS_MAINLINE, &head_sha)
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

    /// Bootstrap (idempotently) `bloom`'s integration namespace at `base` and
    /// return the branch's current position — where an integration fold starts
    /// or resumes, plus the recovered landable head when the branch has already
    /// advanced (ADR-0152).
    ///
    /// # Errors
    /// The base has no recorded correspondence, or the ref reads/writes failed.
    pub fn integration_checkpoint(&self, bloom: &BloomId, base: &Digest) -> Result<IntegrationPosition, SourceError> {
        self.backend.integration_checkpoint(bloom, base)
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

    use super::{
        Arc, BloomId, Digest, IntegrateOutcome, LandOutcome, Snapshot, SourceBackend, SourceError, SourceShell,
    };

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
    fn genesis_reconcile_seeds_the_base_and_a_real_repo_snapshot_integrate_land_settles() {
        // The #3615 end-to-end criterion over the fake: a boot genesis reconcile
        // seeds `Snapshot::GENESIS_MAINLINE ↔ the repo's real head`, after which
        // the first snapshot → integrate → land resolves with no `Malformed` and
        // no `UnresolvedCorrespondence`. Drives the source shell (resolve is the
        // core reducer's, exercised in the golden trace); the head the reducer
        // would carry to land is the distinct integrated-head digest recorded here.
        let fake = FakeGithub::new();
        let base_tree = Digest::from_bytes([10; 32]);
        // Seed the repo's real head commit + tree object and the mainline ref
        // pointing at the commit, but record NO `base ↔ head` correspondence for
        // the genesis base — the reconcile is what establishes it (a lazy
        // first-snapshot seed would instead equate base and head and defeat CAS
        // base-moved detection). Deliberately not `seed_base_commit`: that would
        // pre-record a *second* digest for the head commit object, and the fake's
        // reverse-resolve scans for any digest naming that object, so `land`'s
        // base check would then non-deterministically read either digest.
        // Genesis must be the sole digest for the head commit.
        let correspondence: SharedCorrespondence = Arc::new(fake.clone());
        fake.seed_git_object(&base_tree);
        let tree_sha = correspondence.resolve_git(&base_tree).unwrap().unwrap().to_hex();
        let head_sha = fake.seed_commit(&tree_sha);
        fake.seed_ref("heads/main", &head_sha);

        let concrete = Arc::new(GitSource::new(fake.clone(), Arc::clone(&correspondence), true));
        let backend: Arc<dyn SourceBackend<Error = SourceError> + Send + Sync> = concrete.clone();
        let bloom = BloomId(Digest::from_bytes([1; 32]));
        let shell = SourceShell { backend, correspondence: Some(Arc::clone(&correspondence)) };

        // Genesis reconcile establishes the initial correspondence authoritatively
        // — the sole digest mapping for the head commit object.
        shell.reconcile_genesis_mainline().unwrap();
        assert!(
            correspondence.resolve_git(&Snapshot::GENESIS_MAINLINE).unwrap().is_some(),
            "the genesis base resolves to the repo's real head after the reconcile"
        );
        // The integration namespace is cut from the genesis base (resolvable only
        // after the reconcile), mirroring a real bloom's boot.
        concrete.create_namespace(&bloom, &Snapshot::GENESIS_MAINLINE).unwrap();

        // The first snapshot at the genesis base now resolves (impossible before
        // the reconcile — the base had no correspondence to forward-resolve).
        let snapshot = shell.snapshot(&Snapshot::GENESIS_MAINLINE).unwrap();
        assert_eq!(snapshot.tree, base_tree, "the genesis snapshot carries the real base tree");

        // Integrate a candidate: the produced commit is the landable head, a
        // distinct digest from the artifact tree.
        let checkpoint = shell.checkpoint(&bloom, &snapshot.tree).unwrap();
        let candidate = Digest::from_bytes([50; 32]);
        fake.seed_git_object(&candidate);
        let head = match shell.integrate(&bloom, &candidate, &checkpoint).unwrap() {
            IntegrateOutcome::Integrated { tree, head } => {
                assert_eq!(tree, candidate);
                assert_ne!(head, tree, "the integrated head is distinct from the artifact tree");
                head
            }
            other => panic!("expected Integrated, got {other:?}"),
        };

        // Land the integrated head against the genesis base — the reverse-resolve
        // of the real mainline object returns the genesis base and the CAS
        // proceeds, settling with no Malformed / UnresolvedCorrespondence fault.
        match shell.land(&bloom, &Snapshot::GENESIS_MAINLINE, &head).unwrap() {
            LandOutcome::Landed(receipt) => {
                assert_eq!(receipt.previous_base, Snapshot::GENESIS_MAINLINE);
                assert_eq!(receipt.new_head, head, "mainline advanced onto the integrated head commit");
            }
            LandOutcome::BaseMoved { expected, actual } => {
                panic!("expected Landed, got BaseMoved {{ expected: {expected:?}, actual: {actual:?} }}")
            }
        }
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
