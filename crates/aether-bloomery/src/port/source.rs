//! The source port (ADR-0149 §The boundary): snapshot, checkpoint,
//! integrate, and compare-and-swap land. Branch names are working handles,
//! never identity — every value here is digest-addressed.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{BloomId, WorkpieceId};
use crate::values::LandingReceipt;

/// A snapshot of the source at a base: its head and tree digests.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SourceSnapshot {
    /// The base head digest the snapshot was taken at.
    pub head: Digest,
    /// The tree digest at that head.
    pub tree: Digest,
}

/// A per-bloom integration checkpoint — a single-writer integration branch's
/// current tree, reusable across a drift-induced supersession.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The bloom this checkpoint belongs to.
    pub bloom: BloomId,
    /// The integrated tree at the checkpoint.
    pub tree: Digest,
}

/// An integration branch's current position, as
/// [`integration_checkpoint`](SourceBackend::integration_checkpoint) reads it:
/// the checkpoint the next fold's CAS expects, and — when the branch's current
/// commit reverse-resolves to a recorded landable head — that head digest, so a
/// fold interrupted after its final integrate recovers the head for its resolve
/// instead of wedging (ADR-0152).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct IntegrationPosition {
    /// The branch's current checkpoint.
    pub checkpoint: Checkpoint,
    /// The landable head the current commit reverse-resolves to; `None` at the
    /// freshly-bootstrapped base (whose commit resolves to the base digest, not
    /// a head) or when no correspondence is recorded.
    pub head: Option<Digest>,
}

/// The outcome of integrating one candidate onto a bloom's integration
/// branch.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum IntegrateOutcome {
    /// The candidate integrated; the branch now carries `tree`.
    Integrated {
        /// The resulting integrated tree.
        tree: Digest,
        /// The landable head commit's digest, distinct from the artifact
        /// `tree`. The integrator builds a real commit over `tree` and records
        /// `head ↔ commit`; carrying `head` separately keeps the `tree`
        /// correspondence (which `StaleCheckpoint`/`snapshot` reverse-read)
        /// from being clobbered when the produced commit is recorded, so
        /// `land` can CAS mainline onto a commit rather than a bare tree.
        head: Digest,
    },
    /// The candidate conflicted at `at` and was not integrated.
    Conflict {
        /// The conflicting tree/point.
        at: Digest,
    },
    /// The expected checkpoint was stale — the integration branch has
    /// advanced past it, so the single-writer CAS is refused rather than
    /// clobbering a concurrent advance. The caller re-reads the current
    /// checkpoint and retries.
    StaleCheckpoint {
        /// The tree the integration branch is actually at.
        actual: Digest,
    },
}

/// The outcome of issuing a land. If mainline is no longer the sealed base
/// the land is refused — the bloom is not rebased under its evidence; a
/// successor seals on the new head (ADR-0149 §The bloom).
///
/// Landing is a *two-step* on a protected mainline: Bloomery proposes the
/// resolved head, something outside Bloomery accepts it, and the landing is
/// observed afterwards. So issuing a land can only ever report "proposed" or
/// "refused" — the landed case lives on [`LandProposal`], which is what
/// observing produces. Neither enum carries a variant its producer cannot
/// return.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LandOutcome {
    /// The resolved head was proposed; mainline has not moved yet. The caller
    /// records the proposal and watches it to its terminal.
    Proposed {
        /// The proposal's number on the backend — the handle a watch re-reads
        /// it by.
        number: u64,
    },
    /// The land was refused: mainline had moved off the expected base.
    BaseMoved {
        /// The base the caller expected mainline to still be at.
        expected: Digest,
        /// The base mainline was actually at.
        actual: Digest,
    },
}

/// Where an issued land proposal has got to — the three states a watch
/// distinguishes, each routing the bloom somewhere different.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LandProposal {
    /// Still open. Mainline has not moved; keep watching.
    Open,
    /// Accepted — mainline moved, and the receipt says where to.
    ///
    /// The receipt's `new_head` is what mainline *became*, which on a
    /// squash-accepting backend is not the head that was proposed. A watch
    /// records this value rather than re-deriving it from the proposal, or it
    /// would attest a mainline commit that exists nowhere.
    Landed(LandingReceipt),
    /// Terminated without landing — the proposal was declined. The bloom stays
    /// resolved and supersedable, the same terminal
    /// [`LandOutcome::BaseMoved`] reaches; it is a distinct variant because the
    /// cause is an operator decision rather than a moved base, and an operator
    /// reading the log needs to tell those apart.
    Declined,
}

/// Which claim ref a [`ClaimOutcome::Held`] conflict is on (ADR-0150 §The claim
/// registry): a per-workpiece claim ref, or the single mainline-admission ref.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ClaimRefKind {
    /// The per-workpiece claim ref (`refs/bloomery/claims/<workpiece-id>`).
    Workpiece(WorkpieceId),
    /// The single mainline-admission ref (`refs/bloomery/admission/mainline`) —
    /// Bloomery's "one sealed, unlanded bloom per mainline" invariant as a
    /// shared repository ref.
    MainlineAdmission,
}

/// The outcome of an all-or-nothing claim acquire, transfer, or release over the
/// commit-chain claim refs (ADR-0150 §The claim registry, amended PR #3539).
/// Mirrors [`LandOutcome`]: a clean [`Held`] refusal is not an error.
///
/// The refs are realized as **commit-chain claim handles** so a supersession
/// transfers a claim by fast-forward compare-and-swap rather than a race-prone
/// release-then-re-acquire, and a release linearizes on a CAS-to-tombstone
/// rather than a check-then-delete.
///
/// [`Held`]: ClaimOutcome::Held
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ClaimOutcome {
    /// Every targeted ref was acquired / transferred / released.
    Acquired,
    /// A targeted ref was already held by another bloom, so the operation was
    /// refused — an acquire that hits this rolls back every ref it created, a
    /// transfer's fast-forward CAS lost cleanly (the ref is never momentarily
    /// absent), and a release spared a ref it does not own. Reports the first
    /// conflicting ref and its holder.
    Held {
        /// Which ref the conflict was on.
        ref_kind: ClaimRefKind,
        /// The bloom currently holding it.
        held_by: BloomId,
    },
}

/// Who a live claim ref points at, as [`enumerate_claims`](SourceBackend::enumerate_claims)
/// classifies it (ADR-0150 §The claim registry, amended PR #3556). An absent ref
/// is not a holder — it is simply not enumerated.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ClaimHolder {
    /// The ref points at a live claim commit whose tree is this bloom's id.
    Held(BloomId),
    /// The ref points at the all-zero tombstone commit an interrupted
    /// `release_seal` left after its CAS-to-tombstone linearized but its
    /// name-only cleanup delete did not run — a sweep-me marker, not a holder.
    Tombstoned,
}

/// One enumerated claim ref: which ref it is and who holds it (ADR-0150 §The
/// claim registry, amended PR #3556). The boot reconcile folds a
/// [`Vec<ClaimRefState>`](ClaimRefState) into its deep heals — a
/// [`Tombstoned`](ClaimHolder::Tombstoned) ref is swept, and a ref
/// [`Held`](ClaimHolder::Held) by a superseded predecessor is driven to its
/// successor (or released if the successor dropped it).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ClaimRefState {
    /// Which claim ref this is.
    pub ref_kind: ClaimRefKind,
    /// Who currently holds it.
    pub holder: ClaimHolder,
}

/// The versioning-substrate boundary. Git remains the substrate; this port is
/// how Bloomery reads and advances it (ADR-0149 §The boundary). The
/// implementation does the I/O — this trait is the contract.
pub trait SourceBackend {
    /// The backend's error type.
    type Error;

    /// Snapshot the source at `base`.
    ///
    /// # Errors
    /// Backend-defined — e.g. `base` is unknown or the source is unreachable.
    fn snapshot(&self, base: &Digest) -> Result<SourceSnapshot, Self::Error>;

    /// Read the repository's live mainline head as a raw git object sha,
    /// resolving no correspondence. This is the genesis reconcile's entry point
    /// (issue #3615): at boot the mainline base digest has no recorded
    /// correspondence yet, so `snapshot` — which forward-resolves the base
    /// digest to a sha — cannot run; the reconcile reads the real head sha here
    /// and seeds `Snapshot::default().mainline ↔ that sha` before the first
    /// snapshot.
    ///
    /// # Errors
    /// Backend-defined — e.g. the mainline ref is missing or the source is
    /// unreachable.
    fn mainline_head_sha(&self) -> Result<String, Self::Error>;

    /// Record an integration checkpoint for `bloom` at `tree`.
    ///
    /// # Errors
    /// Backend-defined — e.g. the integration branch could not be written.
    fn checkpoint(&self, bloom: &BloomId, tree: &Digest) -> Result<Checkpoint, Self::Error>;

    /// Bootstrap (idempotently) `bloom`'s integration namespace at `base` and
    /// return the branch's current position — the checkpoint a first (or
    /// resumed) [`integrate`](Self::integrate) folds against, plus the landable
    /// head the branch's current commit reverse-resolves to when a prior
    /// integrate recorded one (ADR-0152 §Resolution drives integration). A
    /// fresh branch is created at the base commit; an existing branch is left
    /// as is, so a reactor restarting mid-fold reads where the fold stopped —
    /// and a reactor restarting *after* the final fold recovers the head it
    /// needs for the resolve without re-integrating.
    ///
    /// # Errors
    /// Backend-defined — e.g. the base has no recorded correspondence, or the
    /// ref reads/writes failed.
    fn integration_checkpoint(&self, bloom: &BloomId, base: &Digest) -> Result<IntegrationPosition, Self::Error>;

    /// Enumerate the recorded integration checkpoints for `bloom`, so a
    /// successor bloom can reuse the ones drift did not invalidate
    /// (ADR-0149's successor-reuse clause requires checkpoints be queryable
    /// by digest — [#3465]).
    ///
    /// # Errors
    /// Backend-defined — e.g. the integration branch could not be read.
    ///
    /// [#3465]: https://github.com/iamacoffeepot/aether/issues/3465
    fn checkpoints(&self, bloom: &BloomId) -> Result<Vec<Checkpoint>, Self::Error>;

    /// Integrate `candidate` onto `bloom`'s single-writer integration branch,
    /// guarded by `expected`: if the branch has advanced past that checkpoint
    /// the swap is refused with [`IntegrateOutcome::StaleCheckpoint`] rather
    /// than clobbering a concurrent advance (the single-writer CAS semantics
    /// ADR-0149 assigns to integrate).
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the
    /// clean [`IntegrateOutcome::Conflict`] / [`IntegrateOutcome::StaleCheckpoint`]
    /// results.
    fn integrate(
        &self,
        bloom: &BloomId,
        candidate: &Digest,
        expected: &Checkpoint,
    ) -> Result<IntegrateOutcome, Self::Error>;

    /// Propose landing `bloom`'s `new_head` onto mainline, guarded by
    /// `expected_base`: a mainline that has moved off the sealed base is the
    /// clean [`LandOutcome::BaseMoved`] refusal, not an error, and no proposal
    /// is issued.
    ///
    /// The guard runs first and is the compare half of the compare-and-swap
    /// ADR-0149 assigns to landing; the swap half is the backend's own
    /// accept-time conflict check. Issuing is idempotent — a bloom already
    /// proposed reports the same [`LandOutcome::Proposed`] number rather than
    /// proposing again.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the
    /// clean base-moved refusal.
    fn land(&self, bloom: &BloomId, expected_base: &Digest, new_head: &Digest) -> Result<LandOutcome, Self::Error>;

    /// Read where the land proposal `number`, previously issued for `bloom`
    /// against `expected_base`, has got to.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault.
    fn poll_land(&self, bloom: &BloomId, expected_base: &Digest, number: u64) -> Result<LandProposal, Self::Error>;

    /// Acquire `bloom`'s claim refs — one per member `workpieces` plus the
    /// single mainline-admission ref — all-or-nothing (ADR-0150 §The claim
    /// registry). Each is created as a fresh commit-chain claim handle; a ref
    /// that already exists is another bloom's hold, reported as the first
    /// [`ClaimOutcome::Held`] with the whole acquire rolled back so no partial
    /// claim survives.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    fn claim_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, Self::Error>;

    /// Transfer the seal from `predecessor` to `successor` on a supersession
    /// (ADR-0150 §The claim registry, amended PR #3539): fast-forward compare-
    /// and-swap the `carried` workpiece refs and the admission ref from
    /// predecessor to successor, fresh-acquire the successor's `net_new`
    /// workpieces, and release the `dropped` ones. A carried ref a concurrent
    /// mutation moved off the predecessor loses the CAS cleanly, and a `net_new`
    /// workpiece a foreign bloom holds is a conflict — both the clean
    /// [`ClaimOutcome::Held`], not an error.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the clean
    /// refusal.
    fn transfer_seal(
        &self,
        predecessor: &BloomId,
        successor: &BloomId,
        carried: &[WorkpieceId],
        net_new: &[WorkpieceId],
        dropped: &[WorkpieceId],
    ) -> Result<ClaimOutcome, Self::Error>;

    /// Release `bloom`'s claim refs — the member `workpieces` plus the admission
    /// ref — each by a fast-forward CAS to a tombstone commit (the linearization
    /// point) followed by a name-only cleanup delete (ADR-0150 §The claim
    /// registry, amended PR #3539). The CAS read-guard retires the check-then-
    /// delete TOCTOU: a ref held by another bloom is spared and reported as
    /// [`ClaimOutcome::Held`], never deleted.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the clean
    /// refusal.
    fn release_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, Self::Error>;

    /// Enumerate every live claim ref — the per-workpiece refs plus the
    /// mainline-admission ref — classifying each by its commit tree into a
    /// [`ClaimHolder`] (ADR-0150 §The claim registry, amended PR #3556). Read-
    /// only; the boot reconcile drives it to detect the deep-heal states the V1
    /// re-assert / re-release cannot: a [`Tombstoned`](ClaimHolder::Tombstoned)
    /// ref an interrupted release stranded, and a ref
    /// [`Held`](ClaimHolder::Held) by a superseded predecessor a crashed
    /// `transfer_seal` never moved. An absent ref is not enumerated.
    ///
    /// # Errors
    /// Backend-defined — e.g. the claim-ref namespace could not be read.
    fn enumerate_claims(&self) -> Result<Vec<ClaimRefState>, Self::Error>;

    /// Idempotent per-ref transfer completion (ADR-0150 §The claim registry,
    /// amended PR #3556 — case-3 primitive **(a)**): fast-forward compare-and-swap
    /// one `ref_kind` from `predecessor` to `successor`. A ref at `predecessor`
    /// fast-forwards to `successor` ([`ClaimOutcome::Acquired`]); a ref **already
    /// at `successor` is [`Acquired`](ClaimOutcome::Acquired) — the no-op that
    /// lets a boot re-drive converge**; a ref at any other holder is the clean
    /// [`ClaimOutcome::Held`]. This leaves [`transfer_seal`](Self::transfer_seal)'s
    /// all-or-nothing landed CAS untouched — it re-drives one already-decided
    /// transfer per ref rather than re-attempting the whole op, so a mixed split
    /// (some carried refs at the successor, some still at the predecessor) is
    /// finished without short-circuiting on the first ref already moved.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    fn complete_transfer(
        &self,
        predecessor: &BloomId,
        successor: &BloomId,
        ref_kind: &ClaimRefKind,
    ) -> Result<ClaimOutcome, Self::Error>;

    /// Idempotent per-ref release completion (ADR-0150 §The claim registry,
    /// amended PR #3556) — the per-`ref_kind` form of [`release_seal`](Self::release_seal)
    /// the boot reconcile drives on the enumerated deep-heal refs, freed from
    /// `release_seal`'s fixed member-set-plus-admission signature. Two callers:
    ///
    /// - **Tombstone sweep** — `expected_holder` is `None`: a
    ///   [`Tombstoned`](ClaimHolder::Tombstoned) ref is deleted (finishing an
    ///   interrupted release's name-only cleanup), and a non-tombstoned ref is
    ///   spared as [`ClaimOutcome::Held`] rather than touched. Safe for any
    ///   instance — a tombstone *is* the released state.
    /// - **Stranded-drop release** — `expected_holder` is `Some(bloom)`: a ref
    ///   held by `bloom` is CAS-to-tombstoned then deleted (the same linearizing
    ///   release [`release_seal`](Self::release_seal) runs); an already-tombstoned
    ///   ref is swept; a ref a foreign bloom holds is spared as
    ///   [`ClaimOutcome::Held`]. The `Some(bloom)` guard is the exclusivity check —
    ///   the caller names the holder whose release it is authorizing.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the clean
    /// [`ClaimOutcome::Held`] refusal.
    fn complete_release(
        &self,
        expected_holder: Option<&BloomId>,
        ref_kind: &ClaimRefKind,
    ) -> Result<ClaimOutcome, Self::Error>;
}
