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

/// The outcome of integrating one candidate onto a bloom's integration
/// branch.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum IntegrateOutcome {
    /// The candidate integrated; the branch now carries `tree`.
    Integrated {
        /// The resulting integrated tree.
        tree: Digest,
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

/// The outcome of a compare-and-swap land. If mainline is no longer the
/// sealed base, the swap is refused — the bloom is not rebased under its
/// evidence; a successor seals on the new head (ADR-0149 §The bloom).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LandOutcome {
    /// The swap succeeded; mainline moved and a receipt was issued.
    Landed(LandingReceipt),
    /// The swap was refused: mainline had moved off the expected base.
    BaseMoved {
        /// The base the caller expected mainline to still be at.
        expected: Digest,
        /// The base mainline was actually at.
        actual: Digest,
    },
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

    /// Record an integration checkpoint for `bloom` at `tree`.
    ///
    /// # Errors
    /// Backend-defined — e.g. the integration branch could not be written.
    fn checkpoint(&self, bloom: &BloomId, tree: &Digest) -> Result<Checkpoint, Self::Error>;

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

    /// Compare-and-swap mainline from `expected_base` to `new_head` for
    /// `bloom`. A moved base is the clean [`LandOutcome::BaseMoved`] result,
    /// not an error.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the
    /// clean base-moved refusal.
    fn land(&self, bloom: &BloomId, expected_base: &Digest, new_head: &Digest) -> Result<LandOutcome, Self::Error>;

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
}
