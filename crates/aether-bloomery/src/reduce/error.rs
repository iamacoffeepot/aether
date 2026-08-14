//! Why the reducer refused — one error enum per admission door, each naming
//! the violated rule rather than a bare failure.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{CatalogError, OverrideError, Unproducible};

/// Why an aggregate-review completion was refused (ADR-0153).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AggregateReviewError {
    /// No active bloom with this id.
    UnknownOrInactiveBloom,
    /// The bloom holds no folded integration — no review was dispatched, or a
    /// failing verdict already cleared the stale fold.
    NoPendingIntegration,
    /// The verdict's evidence binds a tree other than the held fold's — a
    /// stale verdict from a superseded fold, never acted on.
    SubjectMismatch {
        /// The held fold's integrated tree.
        expected: Digest,
        /// The tree the verdict's evidence binds.
        got: Digest,
    },
    /// A failing verdict implicated a workpiece that is not a member.
    NotAMember(WorkpieceId),
}

/// Why a landing rejection was refused (#4689).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LandingRejectedError {
    /// No bloom with this id, or it is not awaiting a landing — a rejection
    /// against a bloom that already landed, or never resolved, changes nothing.
    NotAwaitingLanding,
    /// The rejection's evidence binds a head other than the one the bloom is
    /// landing — a stale rejection from a superseded landing, never acted on.
    SubjectMismatch {
        /// The head the bloom is actually landing.
        expected: Digest,
        /// The head the rejection's evidence binds.
        got: Digest,
    },
}

/// Why an aggregate-verify completion was refused.
///
/// The same three refusals the review gate makes about the same held fold,
/// minus an implication check: a compile failure over the fold is a property of
/// the combination, not attributable to one member, so a failing verify carries
/// no implication to validate and re-opens every member.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AggregateVerifyError {
    /// No active bloom with this id.
    UnknownOrInactiveBloom,
    /// The bloom holds no folded integration — nothing was dispatched against,
    /// or a failing verdict already cleared the stale fold.
    NoPendingIntegration,
    /// The verdict's evidence binds a tree other than the held fold's — a
    /// stale verdict from a superseded fold, never acted on.
    SubjectMismatch {
        /// The held fold's integrated tree.
        expected: Digest,
        /// The tree the verdict's evidence binds.
        got: Digest,
    },
}
/// One member already claimed by a foreign active bloom — the conflict that
/// aborts an all-or-nothing seal or supersession.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SealConflict {
    /// The workpiece already claimed.
    pub workpiece: WorkpieceId,
    /// The active bloom holding it.
    pub held_by: BloomId,
}

/// Why a seal was refused (ADR-0149 §The bloom admission rules).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SealError {
    /// A member is already claimed by a foreign active bloom.
    MembershipConflict(SealConflict),
    /// A bloom with this id already exists — re-sealing would resurrect and
    /// overwrite its record, wiping status and claims.
    KnownBloom(BloomId),
    /// The membership set is empty; a bloom with no members would trivially
    /// resolve and advance mainline on zero evidence.
    EmptyMembership,
    /// A workpiece appears more than once in the spec.
    DuplicateWorkpiece(WorkpieceId),
    /// A member's approval does not bind its own scope revision as an
    /// [`EvidenceKind::Approval`](crate::EvidenceKind::Approval).
    UnapprovedMember(WorkpieceId),
    /// A sealed or resolved bloom already occupies the mainline. V1 permits one
    /// unlanded bloom per mainline; a successor seals via supersession instead.
    ActiveBloomExists(BloomId),
    /// The sealing spec sealed a [`StageCatalog`](crate::StageCatalog) the line
    /// cannot run (ADR-0174) — a stage left unbound or bound twice, a process no
    /// executor routes, or a retry budget outside the countable range.
    ///
    /// Refused at the door because the alternative is a member that wedges with
    /// no attempt ever made, long after the operator who authored the catalog has
    /// moved on. Sealing *no* catalog is not this error: it runs the compiled
    /// line, which is structurally valid by construction.
    UnrunnableStageCatalog(CatalogError),
    /// The sealing spec's configuration registry names content the reducer could
    /// not be given (ADR-0174). Either the caller did not fetch it, or what it
    /// fetched is filed under a different kind than the registry key claims.
    ///
    /// Refused at the door rather than at the point of use, because a sealed
    /// address is immutable: content that cannot be produced now will not appear
    /// later, so admitting the bloom would only move the failure to a dispatch
    /// that parks — after the bloom has claimed its members.
    UnproducibleConfig {
        /// The kind the registry key named.
        kind: String,
        /// The address sealed for it.
        address: Digest,
        /// Why the content could not be produced.
        reason: Unproducible,
    },
    /// A member sealed a [`ModelOverride`](crate::ModelOverride) that cannot
    /// apply under the bloom's sealed catalog (#4601) — a stage named twice, or
    /// a stage the catalog binds to no model lane.
    ///
    /// Refused at the door for the reason the whole override exists: an operator
    /// authored a sentence about which model runs where, and an entry nothing
    /// resolves would leave them believing a choice took effect while the lane
    /// ran the calibrated default the receipt does not mention.
    UnusableModelOverride {
        /// The member whose registry sealed it.
        workpiece: WorkpieceId,
        /// What makes the override unusable.
        error: OverrideError,
    },
    /// A member names a workpiece a landed bloom already resolved at the same
    /// scope revision. The journal is the source of truth for what has landed —
    /// GitHub issue state is not: bloom landings squash into the day branch, so a
    /// landed workpiece's source issue stays open until sync-back.
    ///
    /// Refused at the door so an operator cannot pay construct lanes to
    /// fabricate work the operating branch already carries. The re-run escape is
    /// a fresh scope revision for the same workpiece: that pair is not in the
    /// landed set, so a deliberate rework or revert-then-redo is a new approved
    /// plan, not a request flag. Appended so the prior variants' wire
    /// discriminants are unchanged.
    WorkpieceAlreadyLanded {
        /// The workpiece a landed bloom already resolved.
        workpiece: WorkpieceId,
        /// The bloom that landed it.
        bloom: BloomId,
    },
    /// A member names [`WorkpieceId::COMPOSITION`](crate::WorkpieceId::COMPOSITION),
    /// the id reserved for the bloom's synthetic composition workpiece
    /// (ADR-0191).
    ///
    /// Refused at the door because the composition shares the member maps — the
    /// stage cursor, the wedge set, the dispatch ledger — so a member holding
    /// that key would silently share the composition's cursor and each would
    /// move the other's line position. Appended so the prior variants' wire
    /// discriminants are unchanged.
    ReservedWorkpieceId(WorkpieceId),
}

/// Why a supersession was refused.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SupersedeError {
    /// The predecessor is not a known bloom, or is no longer supersedable —
    /// only `Sealed` and `Resolved` blooms supersede.
    UnknownOrInactivePredecessor,
    /// The successor's id equals the predecessor: a bloom cannot supersede
    /// itself into a bloom superseded by itself.
    SelfSupersession,
    /// A bloom with this id already exists (distinct from the predecessor) —
    /// admitting it would resurrect and overwrite that bloom's record, wiping
    /// status and claims, mirroring [`SealError::KnownBloom`].
    KnownSuccessor(BloomId),
    /// A successor member is already claimed by a foreign active bloom (the
    /// predecessor's own holds, released in the same decision set, are exempt).
    MembershipConflict(SealConflict),
    /// The successor's membership fails the same per-member admission a seal
    /// runs — empty, a duplicate workpiece, or an approval that does not bind
    /// its scope revision. A superseding spec is held to seal's member validity.
    InvalidMember(SealError),
    /// The successor rebases onto a head the source never observed (#4709).
    ///
    /// A supersession may move mainline, so the bases it accepts are exactly
    /// two: the current one, and the head the source last reported. Anything
    /// else would let a caller name the compare-and-swap anchor whatever it
    /// likes, and a bloom would land against a head nobody ever saw.
    UnobservedBase {
        /// The base the successor sealed.
        base: Digest,
        /// The head the source last reported — the only base other than current
        /// mainline a successor may take.
        observed: Digest,
    },
}

/// Why an integration was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum IntegrateError {
    /// The bloom is not known or not active.
    UnknownOrInactiveBloom,
    /// The claim's workpiece is not a member of the bloom.
    NotAMember,
    /// The claim's evidence does not bind to the claim's candidate — no
    /// evidence validates a digest it does not name.
    EvidenceNotBound,
}

/// Why an evidence admission was refused (ADR-0151).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AdmitEvidenceError {
    /// The bloom is not known or not active (only a `Sealed` bloom admits
    /// evidence — a resolved, landed, or superseded bloom is past recording).
    UnknownOrInactiveBloom,
    /// The evidence does not bind to its own subject — no evidence validates a
    /// digest it does not name (ADR-0149 §The value vocabulary).
    EvidenceNotBound,
}

/// Why a resolve was refused.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ResolveError {
    /// The bloom is not known or not active.
    UnknownOrInactiveBloom,
    /// A member has no recorded resolution claim yet.
    MemberNotIntegrated {
        /// The unresolved member.
        workpiece: WorkpieceId,
    },
    /// A member's stage is held on a parked question that no answer has released
    /// yet (ADR-0151) — a bloom with a held member cannot resolve.
    PendingDecision {
        /// An open question digest holding the bloom.
        question: Digest,
    },
    /// The bloom has consumed its aggregate-review ceiling (ADR-0153): a fold
    /// arriving past the two-pass budget is refused fail-closed rather than
    /// buying a roll the vocabulary forbids. Appended so the prior variants'
    /// wire discriminants are unchanged.
    ReviewCeiling {
        /// The verdicts already consumed.
        rolls: u32,
    },
}

/// Why an answer adoption was refused (ADR-0151).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AdoptAnswerError {
    /// The bloom is not known or not active (only a `Sealed` bloom holds a
    /// pending decision — a resolved, landed, or superseded bloom is past it).
    UnknownOrInactiveBloom,
    /// The answer statement is not instruction-capable — only an author
    /// signature can become intent (ADR-0149 §The value vocabulary), so a
    /// non-author statement can never adopt a question.
    NotInstructionCapable,
    /// The answer's parents name no open hold on this bloom — an answer adopts
    /// a question by naming its exact digest; one that names no held question
    /// releases nothing.
    NoMatchingHold,
}

/// Why an attempt completion was refused (ADR-0149 §The line).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AttemptCompletedError {
    /// The bloom is not known or not active (only a `Sealed` bloom runs a line —
    /// a resolved, landed, or superseded bloom is past dispatch).
    UnknownOrInactiveBloom,
    /// The completion names a workpiece that is not a member of the bloom.
    NotAMember(WorkpieceId),
    /// The completed stage is not the member's current cursor stage — a stale,
    /// duplicated, or out-of-order attempt result the reducer will not act on.
    /// (A resent idempotency key is caught earlier as [`Outcome::Duplicate`](crate::Outcome::Duplicate); this
    /// is a *different* result naming a stage the member has already left.)
    StageMismatch {
        /// The stage the member's cursor currently sits at.
        expected: StageId,
        /// The stage the completion named.
        got: StageId,
    },
    /// The named stage is terminal `Verify` (or otherwise off the dispatched
    /// member line): passing Verify integrates through
    /// [`Fact::Integrate`](crate::Fact::Integrate), while failing Verify uses
    /// [`Fact::VerifyFailed`](crate::Fact::VerifyFailed), so neither completes here.
    TerminalStage(StageId),
    /// The member holds no stage cursor — it never entered the dispatched line
    /// (a successor member that arrived already integrated as an inherited
    /// claim), so no attempt can complete for it. Distinct from
    /// [`StageMismatch`](Self::StageMismatch): a missing cursor is not a cursor
    /// at the entry stage (#3663). Appended so the prior variants' wire
    /// discriminants are unchanged.
    NotDispatched(WorkpieceId),
}

/// Why a typed terminal-Verify failure was refused (ADR-0178).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum VerifyFailedError {
    /// No active bloom with this id.
    UnknownOrInactiveBloom,
    /// The fact names a workpiece that is not a member of the bloom.
    NotAMember(WorkpieceId),
    /// The member holds no dispatched cursor.
    NotDispatched(WorkpieceId),
    /// The member is not currently waiting at terminal Verify.
    StageMismatch {
        /// The stage the member is actually waiting at.
        expected: StageId,
    },
    /// A failed Verify verdict must name at least one closed verifier identity.
    EmptyFailures,
    /// The evidence does not bind the member's current Verify subject.
    EvidenceNotBound {
        /// The candidate tree, or scope revision before a candidate exists.
        expected: Digest,
        /// The subject the evidence actually names.
        got: Digest,
    },
}

/// Why an attempt grant was refused (#4708).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum GrantAttemptsError {
    /// The bloom is not known or not active — only a `Sealed` bloom runs a line,
    /// so only one can hold a member to resume.
    UnknownOrInactiveBloom,
    /// The grant names a workpiece that is not a member of the bloom.
    NotAMember(WorkpieceId),
    /// The member is not wedged, so there is nothing to hand back. A running
    /// member already holds attempts — re-dispatching it would put two workers on
    /// one workpiece — and a member that never entered the line (an inherited
    /// claim, which holds no cursor) has no position to resume from.
    NotWedged(WorkpieceId),
    /// The grant names a stage other than the one the member wedged at — an
    /// operator acting on a stale read of the projection.
    StageMismatch {
        /// The stage the member is actually wedged at.
        wedged_at: StageId,
        /// The stage the grant named.
        got: StageId,
    },
    /// The grant asks for no attempts, or for more than the member could spend.
    ///
    /// The counters a grant writes are read against the stage's own
    /// [`retry_budget`](crate::StageBinding::retry_budget), so a larger request
    /// could not be spent even if it were admitted. Zero is refused for the
    /// opposite reason: it would move the cursor and dispatch an attempt while
    /// granting nothing, so the member would wedge again on the same verdict.
    BeyondCap {
        /// The attempts asked for.
        requested: u32,
        /// The most this grant may hand back.
        cap: u32,
    },
}

/// Why an orphan-claim release request was refused (ADR-0179).
///
/// Every variant is a synchronous refusal that attempts no mutation: the request
/// fact is never admitted, so no outbox effect is emitted and the source is never
/// dialled.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OrphanClaimReleaseError {
    /// The authorization is not an author signature — only that provenance
    /// becomes instruction (ADR-0149 §The value vocabulary).
    NotInstructionCapable,
    /// The authorization does not bind this request: its words are not the exact
    /// [`ORPHAN_CLAIM_RELEASE_WORDS`](crate::ORPHAN_CLAIM_RELEASE_WORDS), or its
    /// parents do not name the request digest. Both halves matter — without the
    /// parent binding, a signature over those words would authorize the release
    /// of *any* ref.
    AuthorizationNotBound,
    /// The expected holder is a bloom this journal knows, so it is not an orphan.
    /// Boot reconcile, supersession, and the ordinary land-time release own a
    /// known record; this escape hatch must never become a second route around
    /// them.
    HolderKnown(BloomId),
    /// A completion named a request that was never admitted.
    UnknownRequest(Digest),
    /// A completion named a request that already reached a terminal result. The
    /// first completion wins; a second is a stale redrive and changes nothing.
    AlreadyCompleted(Digest),
}

/// A land refused because mainline had moved off the bloom's sealed base.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BaseMismatch {
    /// The bloom's sealed base — the only head it may land on.
    pub expected: Digest,
    /// The base mainline was actually at.
    pub actual: Digest,
}

/// Why a fold-conflict admission was refused (ADR-0189).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FoldConflictError {
    /// No active bloom with this id.
    UnknownOrInactiveBloom,
    /// The fact names a workpiece that is not a member of the bloom.
    NotAMember(WorkpieceId),
    /// The member has no resolution claim — nothing was being folded, so
    /// there is no collision to reconcile.
    NotIntegrated(WorkpieceId),
    /// The evidence does not bind the folded checkpoint tree.
    EvidenceNotBound {
        /// The folded tree the collision names.
        expected: Digest,
        /// The subject the evidence actually names.
        got: Digest,
    },
}

/// Why a land was refused (ADR-0149 §The bloom).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LandError {
    /// No bloom with this id is known.
    UnknownBloom(BloomId),
    /// The bloom exists but is not `Resolved`, so it cannot land.
    NotResolved(BloomId),
    /// Mainline moved off the bloom's sealed base — supersession is forced.
    BaseMismatch(BaseMismatch),
}
