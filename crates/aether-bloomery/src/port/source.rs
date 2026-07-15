//! The source port (ADR-0149 §The boundary): snapshot, checkpoint,
//! integrate, and compare-and-swap land. Branch names are working handles,
//! never identity — every value here is digest-addressed.

use crate::digest::Digest;
use crate::ids::BloomId;
use crate::values::LandingReceipt;

/// A snapshot of the source at a base: its head and tree digests.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceSnapshot {
    /// The base head digest the snapshot was taken at.
    pub head: Digest,
    /// The tree digest at that head.
    pub tree: Digest,
}

/// A per-bloom integration checkpoint — a single-writer integration branch's
/// current tree, reusable across a drift-induced supersession.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Checkpoint {
    /// The bloom this checkpoint belongs to.
    pub bloom: BloomId,
    /// The integrated tree at the checkpoint.
    pub tree: Digest,
}

/// The outcome of integrating one candidate onto a bloom's integration
/// branch.
#[derive(Clone, PartialEq, Eq, Debug)]
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
}

/// The outcome of a compare-and-swap land. If mainline is no longer the
/// sealed base, the swap is refused — the bloom is not rebased under its
/// evidence; a successor seals on the new head (ADR-0149 §The bloom).
#[derive(Clone, PartialEq, Eq, Debug)]
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

    /// Integrate `candidate` onto `bloom`'s single-writer integration branch.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the
    /// clean [`IntegrateOutcome::Conflict`] result.
    fn integrate(&self, bloom: &BloomId, candidate: &Digest) -> Result<IntegrateOutcome, Self::Error>;

    /// Compare-and-swap mainline from `expected_base` to `new_head` for
    /// `bloom`. A moved base is the clean [`LandOutcome::BaseMoved`] result,
    /// not an error.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the
    /// clean base-moved refusal.
    fn land(&self, bloom: &BloomId, expected_base: &Digest, new_head: &Digest) -> Result<LandOutcome, Self::Error>;
}
