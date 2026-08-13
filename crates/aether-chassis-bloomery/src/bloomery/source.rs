//! The git source-port cap shell ([#3465]).
//!
//! The host mounts the `aether-bloomery-github` [`GitSource`] backend behind a
//! thin shell holding it as an `Arc<dyn LandingSource>`, exactly mirroring the
//! [`ProjectionShell`](super::ProjectionShell) — no GitHub type crosses into a
//! core module: the shell is the boundary, and only it and the adapter name a
//! github-crate type (ADR-0149 §The boundary, the "no core module names a
//! GitHub type" clause).
//!
//! At the chassis edge, [`GithubConnectionConfig`] supplies the adapter values
//! used to construct this shell. Consumers receive clones of the constructed
//! shell rather than adapter configuration.
//!
//! This slice ships the shell and the demo that drives a synthetic bloom
//! through it (see `tests/source_demo.rs`). Wiring the shell into the chassis
//! boot as an integrate/land-driving capability lands with the migration
//! step 2 executor/review bridge, when the reactor that consumes it exists —
//! mirroring the mirror shell's staging.
//!
//! [#3465]: https://github.com/iamacoffeepot/aether/issues/3465

use std::sync::Arc;

use aether_bloomery::{
    BackendObjectId, BloomId, Checkpoint, ClaimOutcome, ClaimRefKind, ClaimRefState, ClaimReleaseOutcome,
    CorrespondenceError, Digest, IntegrateOutcome, IntegrationPosition, LandOutcome, LandProposal,
    SharedCorrespondence, Snapshot, SourceSnapshot, WorkpieceId,
};
use aether_bloomery_github::{GitObjectId, GitSource, GithubError, LandingProposal, LandingSource, SourceError};

use super::{CoordinatorConfig, GithubConnectionConfig};

/// The source cap shell: the git source backend behind an `Arc<dyn …>`, so no
/// core module ever names the concrete github-crate type. A live (connected)
/// shell also holds the correspondence handle so it can seed the mainline
/// correspondence (ADR-0150); a fake-backed shell (`new`) seeds the double
/// directly and carries none.
///
/// The backend is held as the adapter's [`LandingSource`], which is an
/// [`aether_bloomery::SourceBackend`] plus the landing-assembly face: the land reactor is what
/// holds a shell and what assembles a proposal's prose, so widening the shell
/// here is what keeps that prose out of the digest-only port contract.
#[derive(Clone)]
pub struct SourceShell {
    backend: Arc<dyn LandingSource + Send + Sync>,
    correspondence: Option<SharedCorrespondence>,
}

impl SourceShell {
    /// Mount an arbitrary source backend — the demo mounts a fake-backed one,
    /// production a `ReqwestGithub`-backed one. Carries no seedable correspondence
    /// handle (the fake-backed tests seed their double directly); use
    /// [`connect`](Self::connect) for a live shell that can [`seed_mainline`](Self::seed_mainline).
    #[must_use]
    pub fn new(backend: Arc<dyn LandingSource + Send + Sync>) -> Self {
        Self { backend, correspondence: None }
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn new_with_correspondence(
        backend: Arc<dyn LandingSource + Send + Sync>,
        correspondence: SharedCorrespondence,
    ) -> Self {
        Self { backend, correspondence: Some(correspondence) }
    }

    /// Connect a live GitHub-backed source port from resolved config, over the
    /// persisted correspondence (ADR-0150). The `cas_land_enabled` knob gates
    /// `land` — on by default since ADR-0149 migration step 3 made the CAS `land`
    /// the landing of record; a `false` knob is the explicit kill switch. The
    /// coordinator's `mainline_ref` says which branch every mainline read
    /// addresses (ADR-0186).
    ///
    /// Takes both configs the way [`ExecutorShell::connect`](super::ExecutorShell::connect)
    /// does: the connection knobs say where the port talks, the coordinator knobs
    /// say how this deployment is being operated, and the port needs one of each.
    ///
    /// # Errors
    /// The underlying `reqwest` client or the correspondence store could not be
    /// constructed.
    pub fn connect(
        config: &GithubConnectionConfig,
        coordinator: &CoordinatorConfig,
        correspondence: SharedCorrespondence,
    ) -> Result<Self, GithubError> {
        let client = config.connect_client()?;
        let backend = Arc::new(GitSource::new(
            client,
            Arc::clone(&correspondence),
            config.cas_land_enabled,
            coordinator.mainline(),
        ));
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
        correspondence.record(base, &BackendObjectId::from(git))?;
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
    /// The seed binds **once**. `GENESIS_MAINLINE` names mainline as it stood
    /// when this journal opened, and every bloom sealed before the first land
    /// carries that sentinel as its base — so re-seeding at a later boot
    /// re-points what the sentinel *means* at whatever mainline has since become.
    /// That equates base and head for exactly those blooms and defeats the
    /// base-moved detection this eager read exists to preserve: the lazy-seed
    /// failure above, reintroduced through the restart path. Mainline moves for
    /// reasons that have nothing to do with blooms (any merged pull request), so
    /// a restart after unrelated activity is the common case, not a corner.
    ///
    /// # Errors
    /// The mainline ref is unreachable, no correspondence store is mounted (a
    /// fake-backed shell), the head sha is malformed, or the store read or write
    /// faulted.
    pub fn reconcile_genesis_mainline(&self) -> Result<(), SourceError> {
        let correspondence = self
            .correspondence
            .as_ref()
            .ok_or_else(|| SourceError::Correspondence(CorrespondenceError::new("no correspondence store mounted")))?;
        if let Some(object) = correspondence.resolve_backend_object(&Snapshot::GENESIS_MAINLINE)? {
            GitObjectId::try_from(object)?;
            return Ok(());
        }

        self.seed_mainline(&Snapshot::GENESIS_MAINLINE, &self.backend.mainline_head_sha()?)
    }

    /// Observe the repository's live mainline head as the digest naming it
    /// (#4667) — what a `Fact::ObserveMainline` carries.
    ///
    /// Distinct from [`reconcile_genesis_mainline`](Self::reconcile_genesis_mainline),
    /// which binds a sentinel *once* and must never re-point. This reads the
    /// head every time and names it; the control core decides whether that means
    /// anything. Splitting the two is the point: the genesis seed is a historical
    /// anchor and the observation is a live pointer, and collapsing them into one
    /// re-seeding read is precisely what defeats base-moved detection.
    ///
    /// # Errors
    /// The mainline ref is unreachable, the head sha is malformed, or the
    /// correspondence store faulted.
    pub fn observe_mainline_head(&self) -> Result<Digest, SourceError> {
        self.backend.observe_mainline_head()
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

    /// Fold the candidate at `candidate_ref` in by merging it — what a fold that
    /// must combine work built against different points uses instead of
    /// [`integrate`](Self::integrate)'s tree-replace.
    ///
    /// # Errors
    /// The integration branch or candidate ref is missing, or the source is
    /// unreachable.
    pub fn integrate_merge(
        &self,
        bloom: &BloomId,
        candidate_ref: &str,
        expected: &Checkpoint,
    ) -> Result<IntegrateOutcome, SourceError> {
        self.backend.integrate_merge(bloom, candidate_ref, expected)
    }

    /// Adopt `predecessor`'s candidate ref for `workpiece` into `successor`'s
    /// namespace, so a bloom that inherited the claim can fold the work behind
    /// it. Adopt-if-absent: a ref the successor already carries stands. `false`
    /// when neither namespace holds one.
    ///
    /// # Errors
    /// The ref could not be read or written.
    pub fn adopt_candidate(
        &self,
        predecessor: &BloomId,
        successor: &BloomId,
        workpiece: &str,
    ) -> Result<bool, SourceError> {
        self.backend.adopt_candidate(predecessor, successor, workpiece)
    }

    /// Propose landing `new_head` onto mainline, guarded by `expected_base`,
    /// under the caller's assembled `proposal` prose. `None` opens the proposal
    /// under the adapter's floor title and bare provenance body — what a caller
    /// with no membership in view (the source cap's own surface) issues.
    ///
    /// # Errors
    /// [`SourceError::LandingDisabled`] while the land gate is off, or a
    /// transport/backend fault (a moved base is the clean
    /// [`LandOutcome::BaseMoved`], not an error).
    pub fn land(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        proposal: Option<&LandingProposal>,
    ) -> Result<LandOutcome, SourceError> {
        self.backend.land_proposal(bloom, expected_base, new_head, proposal)
    }

    /// The human-authored title of issue `number`, or `None` when the repository
    /// holds no such object — the landing assembly's fallback for a member whose
    /// lane named no commit message.
    ///
    /// # Errors
    /// A transport or backend fault.
    pub fn issue_title(&self, number: u64) -> Result<Option<String>, SourceError> {
        self.backend.issue_title(number)
    }

    /// Read where a previously issued land proposal has got to.
    ///
    /// # Errors
    /// A transport or backend fault.
    pub fn poll_land(&self, bloom: &BloomId, expected_base: &Digest, number: u64) -> Result<LandProposal, SourceError> {
        self.backend.poll_land(bloom, expected_base, number)
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
    /// holder), release a ref held by `Some(bloom)`, or — the ADR-0179 operator
    /// path — release one orphaned ref against its expected holder.
    ///
    /// # Errors
    /// A transport or backend fault, distinct from every clean
    /// [`ClaimReleaseOutcome`].
    /// Delete `bloom`'s candidate, integration, and checkpoint refs. Claim refs
    /// and the landing branch are spared (ADR-0150 — claims have their own
    /// release reactor).
    ///
    /// # Errors
    /// A transport or backend fault other than an already-absent ref.
    pub fn prune_working_refs(&self, bloom: &BloomId) -> Result<usize, SourceError> {
        self.backend.prune_working_refs(bloom)
    }

    pub fn complete_release(
        &self,
        expected_holder: Option<&BloomId>,
        ref_kind: &ClaimRefKind,
    ) -> Result<ClaimReleaseOutcome, SourceError> {
        self.backend.complete_release(expected_holder, ref_kind)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{BackendObjectId, SharedCorrespondence};
    use aether_bloomery_github::testing::FakeGithub;
    use aether_bloomery_github::{GitObjectId, GitSource, MainlineRef};

    use super::{
        Arc, BloomId, Digest, IntegrateOutcome, LandOutcome, LandProposal, LandingSource, Snapshot, SourceError,
        SourceShell,
    };

    #[test]
    fn seed_mainline_records_a_resolvable_base_correspondence() {
        // Tripwire: seed_mainline records the sealed base digest ↔ the repo's real
        // (40-hex sha1) mainline head commit, so a later forward resolve reads it back
        // — the record path the deferred boot reconcile drives, verified here
        // without the live mainline read that supplies the sha.
        let fake = FakeGithub::new();
        let correspondence: SharedCorrespondence = Arc::new(fake.clone());
        let backend = Arc::new(GitSource::new(fake, Arc::clone(&correspondence), true, MainlineRef::default()));
        let shell = SourceShell { backend, correspondence: Some(Arc::clone(&correspondence)) };

        let base = Digest::from_bytes([7; 32]);
        let head = "3a3f8c0b9e1d2a4f6b8c0e2d4a6f8b0c1e3d5a7f";
        shell.seed_mainline(&base, head).unwrap();

        let resolved = GitObjectId::try_from(
            correspondence
                .resolve_backend_object(&base)
                .unwrap()
                .expect("the seeded base resolves to the mainline head"),
        )
        .unwrap();
        assert_eq!(resolved.to_hex(), head, "the resolved object is the real 40-hex sha1, not a hex-punned digest");
    }

    #[test]
    fn a_later_boot_leaves_genesis_bound_to_the_head_the_journal_opened_against() {
        // `GENESIS_MAINLINE` names mainline as it stood when this journal opened,
        // and every bloom sealed before the first land carries that sentinel as
        // its base. A boot that re-seeded would re-point what the sentinel means
        // at whatever mainline has since become — equating base and head for
        // exactly those blooms, so `land` reads an unmoved base and lands them on
        // a mainline that moved underneath them. Mainline advances on any merged
        // pull request, so a restart after unrelated activity is the ordinary
        // case rather than a corner.
        let fake = FakeGithub::new();
        let correspondence: SharedCorrespondence = Arc::new(fake.clone());
        let tree = Digest::from_bytes([10; 32]);
        fake.seed_git_object(&tree);
        let tree_sha =
            GitObjectId::try_from(correspondence.resolve_backend_object(&tree).unwrap().unwrap()).unwrap().to_hex();

        let opening_head = fake.seed_commit_with_message("opening", &tree_sha);
        fake.seed_ref("heads/main", &opening_head);
        let backend = Arc::new(GitSource::new(fake.clone(), Arc::clone(&correspondence), true, MainlineRef::default()));
        let shell = SourceShell { backend, correspondence: Some(Arc::clone(&correspondence)) };
        shell.reconcile_genesis_mainline().unwrap();

        // Mainline moves for a reason that has nothing to do with a bloom, then
        // the coordinator restarts and runs its boot reconcile again.
        let moved_head = fake.seed_commit_with_message("someone else's merge", &tree_sha);
        fake.seed_ref("heads/main", &moved_head);
        assert_ne!(moved_head, opening_head, "test: mainline genuinely moved");
        shell.reconcile_genesis_mainline().unwrap();

        assert_eq!(
            GitObjectId::try_from(
                correspondence.resolve_backend_object(&Snapshot::GENESIS_MAINLINE).unwrap().unwrap(),
            )
            .unwrap()
            .to_hex(),
            opening_head,
            "the later boot leaves genesis bound to the head the journal opened against",
        );
    }

    #[test]
    fn genesis_binds_to_the_configured_mainline_ref_rather_than_the_default_branch() {
        // Tripwire: the base every bloom seals against is whatever the configured
        // mainline ref holds (ADR-0186). A reconcile that read the default branch
        // on a repointed coordinator would bind `GENESIS_MAINLINE` to a commit the
        // day branch never held, and the first land's base check would then refuse
        // every bloom of the day as moved.
        let fake = FakeGithub::new();
        let correspondence: SharedCorrespondence = Arc::new(fake.clone());
        let tree = Digest::from_bytes([10; 32]);
        fake.seed_git_object(&tree);
        let tree_sha =
            GitObjectId::try_from(correspondence.resolve_backend_object(&tree).unwrap().unwrap()).unwrap().to_hex();

        let day = MainlineRef::new("refs/heads/bloomery/daily/2026-08-13");
        let day_head = fake.seed_commit_with_message("the day branch", &tree_sha);
        fake.seed_ref(day.git_ref(), &day_head);
        fake.seed_ref("heads/main", &fake.seed_commit_with_message("main moved on", &tree_sha));

        let backend = Arc::new(GitSource::new(fake, Arc::clone(&correspondence), true, day));
        let shell = SourceShell { backend, correspondence: Some(Arc::clone(&correspondence)) };
        shell.reconcile_genesis_mainline().unwrap();

        assert_eq!(
            GitObjectId::try_from(
                correspondence.resolve_backend_object(&Snapshot::GENESIS_MAINLINE).unwrap().unwrap(),
            )
            .unwrap()
            .to_hex(),
            day_head,
            "the sealing base is the head of the branch the coordinator operates on",
        );
    }

    #[test]
    fn genesis_reconcile_rejects_a_non_git_backend_binding() {
        let fake = FakeGithub::new();
        let correspondence: SharedCorrespondence = Arc::new(fake.clone());
        correspondence.record(&Snapshot::GENESIS_MAINLINE, &BackendObjectId::new(vec![0xAB; 17])).unwrap();
        let backend = Arc::new(GitSource::new(fake, Arc::clone(&correspondence), true, MainlineRef::default()));
        let shell = SourceShell { backend, correspondence: Some(correspondence) };

        assert!(
            matches!(shell.reconcile_genesis_mainline(), Err(SourceError::Correspondence(_))),
            "an opaque binding that cannot cross the Git adapter boundary must not suppress authoritative seeding",
        );
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
        let tree_sha = GitObjectId::try_from(correspondence.resolve_backend_object(&base_tree).unwrap().unwrap())
            .unwrap()
            .to_hex();
        let head_sha = fake.seed_commit(&tree_sha);
        fake.seed_ref("heads/main", &head_sha);

        let concrete =
            Arc::new(GitSource::new(fake.clone(), Arc::clone(&correspondence), true, MainlineRef::default()));
        let backend: Arc<dyn LandingSource + Send + Sync> = concrete.clone();
        let bloom = BloomId(Digest::from_bytes([1; 32]));
        let shell = SourceShell { backend, correspondence: Some(Arc::clone(&correspondence)) };

        // Genesis reconcile establishes the initial correspondence authoritatively
        // — the sole digest mapping for the head commit object.
        shell.reconcile_genesis_mainline().unwrap();
        assert!(
            correspondence.resolve_backend_object(&Snapshot::GENESIS_MAINLINE).unwrap().is_some(),
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

        // Propose landing the integrated head against the genesis base — the
        // reverse-resolve of the real mainline object returns the genesis base,
        // so the guard passes and a proposal is opened with no Malformed /
        // UnresolvedCorrespondence fault.
        let number = match shell.land(&bloom, &Snapshot::GENESIS_MAINLINE, &head, None).unwrap() {
            LandOutcome::Proposed { number } => number,
            LandOutcome::BaseMoved { expected, actual } => {
                panic!("expected Proposed, got BaseMoved {{ expected: {expected:?}, actual: {actual:?} }}")
            }
        };
        assert_eq!(shell.poll_land(&bloom, &Snapshot::GENESIS_MAINLINE, number).unwrap(), LandProposal::Open);

        // The operator merges it, and the receipt attests the commit mainline
        // actually became — closing the whole genesis-to-receipt path the
        // reactor drives.
        fake.merge_pull_request(number, &"5c".repeat(20));
        let landed = shell.poll_land(&bloom, &Snapshot::GENESIS_MAINLINE, number).unwrap();
        let LandProposal::Landed(receipt) = landed else {
            panic!("expected Landed, got {landed:?}")
        };
        assert_eq!(receipt.previous_base, Snapshot::GENESIS_MAINLINE);
        assert_ne!(receipt.new_head, head, "the landed head is the merge commit, not the proposed head");
    }

    #[test]
    fn seed_mainline_without_a_mounted_correspondence_errors() {
        // Tripwire: a fake-backed shell (`new`) carries no correspondence store, so
        // seed_mainline refuses cleanly rather than silently no-op-ing the record.
        let fake = FakeGithub::new();
        let shell =
            SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake), true, MainlineRef::default())));
        assert!(shell.seed_mainline(&Digest::from_bytes([7; 32]), "3a3f8c0b9e1d2a4f6b8c0e2d4a6f8b0c1e3d5a7f").is_err());
    }
}
