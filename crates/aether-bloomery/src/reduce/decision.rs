//! The reducer's effect vocabulary. A [`Decision`] is either snapshot-folding
//! (it evolves the projection) or snapshot-inert (it carries an outbox row the
//! host drains and turns into I/O) — the reducer never does I/O itself.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{FoldedIntegration, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, LandingReceipt, ResolutionClaim, ResolvedBloom, Transformation};

/// The ordered effects a decision applies to the projection (and, in
/// production, the outbox/store).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Decision {
    /// Claim a workpiece's active membership for a bloom.
    ClaimMembership {
        /// The claimed workpiece.
        workpiece: WorkpieceId,
        /// The claiming bloom.
        bloom: BloomId,
    },
    /// Release a workpiece's active membership from a bloom.
    ReleaseMembership {
        /// The released workpiece.
        workpiece: WorkpieceId,
        /// The bloom the claim is released from.
        bloom: BloomId,
    },
    /// Inherit a predecessor's resolution claim into a successor.
    InheritClaim {
        /// The successor inheriting the claim.
        bloom: BloomId,
        /// The inherited claim.
        claim: ResolutionClaim,
    },
    /// Record a resolution claim on a bloom (from integration).
    RecordResolution {
        /// The bloom the claim is recorded on.
        bloom: BloomId,
        /// The recorded claim.
        claim: ResolutionClaim,
    },
    /// Append non-integrating evidence to a bloom's evidence log (from
    /// admission). A [`EvidenceKind::Question`](crate::EvidenceKind::Question) entry additionally folds its
    /// `detail` digest into the record's open holds (see [`BloomRecord::holds`](crate::BloomRecord::holds)).
    RecordEvidence {
        /// The bloom the evidence is recorded on.
        bloom: BloomId,
        /// The admitted evidence.
        evidence: Evidence,
    },
    /// Mark a bloom superseded by a successor.
    MarkSuperseded {
        /// The superseded bloom.
        bloom: BloomId,
        /// The successor.
        by: BloomId,
    },
    /// Store a bloom's resolved artifact and mark it resolved.
    SetResolved {
        /// The resolved bloom's id.
        bloom: BloomId,
        /// The resolved artifact.
        resolved: ResolvedBloom,
    },
    /// Advance mainline as part of a land.
    AdvanceMainline {
        /// The prior mainline head.
        from: Digest,
        /// The new mainline head.
        to: Digest,
    },
    /// Emit a landing receipt to the outbox.
    EmitReceipt(LandingReceipt),
    /// Release a member's pending-decision hold (from an adopted answer) —
    /// removes the named question digest from the bloom's open holds so the
    /// bloom can resolve once every member is integrated. Appended so the prior
    /// decisions' wire discriminants are unchanged.
    ReleaseHold {
        /// The bloom the hold is released on.
        bloom: BloomId,
        /// The released question's digest.
        question: Digest,
    },
    /// Re-dispatch a held stage with the adopted answer in its input closure
    /// (from an adopted answer). A snapshot-inert outbox effect — like
    /// [`Decision::EmitReceipt`], it carries no store-projection row and is
    /// republished to the dispatch reactor, which re-assembles the attempt's
    /// prompt manifest naming both the question and the answer digests
    /// (ADR-0151).
    RedispatchStage {
        /// The bloom whose held stage is re-dispatched.
        bloom: BloomId,
        /// The question whose hold was released.
        question: Digest,
        /// The adopting answer's digest — grounds the re-dispatched attempt's
        /// instruction slot.
        answer: Digest,
        /// The answer statement's exact asserted bytes, forwarded so the host
        /// can overlay the decision onto the re-dispatched lane's advisory
        /// channel (#3664). The reducer resolves nothing here — it holds the
        /// adopting [`Statement`](crate::Statement) already, and a lane
        /// re-dispatched without the decision that released it re-parks on the
        /// same question. Carrying content is the
        /// [`Decision::DispatchAttempt`] precedent, not an exception to it.
        words: Vec<u8>,
    },
    /// Dispatch an attempt of `stage` against `workpiece`'s subject in `bloom` —
    /// the transactional-outbox intent the host drains and submits through the
    /// executor port (ADR-0149 §The line / §The boundary). The reducer *decides*
    /// to dispatch; it never does I/O. A snapshot-inert outbox effect like
    /// [`Decision::EmitReceipt`] / [`Decision::RedispatchStage`] — it carries no
    /// in-snapshot state and is rebuilt on replay from the journaled fact.
    /// Appended so the prior decisions' wire discriminants are unchanged.
    DispatchAttempt {
        /// The bloom the dispatched member belongs to.
        bloom: BloomId,
        /// The member workpiece the attempt runs against.
        workpiece: WorkpieceId,
        /// The stage this attempt executes.
        stage: StageId,
        /// The fully-built portable transformation the host wraps in a work order
        /// (adding the idempotency nonce) and submits through the executor port.
        transformation: Transformation,
        /// The member's frozen scope-revision digest, carried explicitly so the
        /// host reactor records it without inferring it from the transformation's
        /// inputs (ADR-0152 — once a candidate exists, `inputs[0]` is the
        /// candidate tree, not the revision).
        scope_revision: Digest,
        /// The candidate tree this attempt runs against, when the member has one
        /// (ADR-0152). The host displays it as the digest returned evidence must
        /// bind to; `None` dispatches against the scope revision (Construct, or
        /// a member with no capture yet).
        candidate: Option<Digest>,
    },
    /// Advance a member's stage cursor to `progress` — the snapshot-folding
    /// counterpart to a [`Decision::DispatchAttempt`]. Overwrites the member's
    /// entry in the record's progress map (see [`BloomRecord::progress`](crate::BloomRecord::progress)); a seal
    /// seeds each member here, a passing attempt moves the cursor forward, a
    /// failing one bumps the attempt count in place.
    AdvanceStage {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member whose cursor advances.
        workpiece: WorkpieceId,
        /// The member's new stage cursor.
        progress: StageProgress,
    },
    /// Drive the source-port compare-and-swap land of a just-resolved bloom —
    /// the transactional-outbox intent the host's land reactor drains and issues
    /// through the source port's `aether.source.land` op (ADR-0149 §The boundary,
    /// migration step 3). Emitted alongside [`Decision::SetResolved`] the moment a
    /// bloom resolves: resolution is land-readiness (a resolved bloom carries its
    /// one artifact and every member's claim), so the land decision rides the same
    /// resolve commit. A snapshot-inert outbox effect like [`Decision::EmitReceipt`]
    /// / [`Decision::DispatchAttempt`] — the actual mainline advance folds in later
    /// from the reactor's [`Fact::Land`](crate::Fact::Land) admit, never from this decision. Appended so
    /// the prior decisions' wire discriminants are unchanged.
    DispatchLand {
        /// The resolving bloom to land.
        bloom: BloomId,
        /// The sealed base the CAS lands on — a moved mainline forces
        /// supersession, never a land onto the new head (ADR-0149 §The bloom).
        expected_base: Digest,
        /// The head mainline advances to on a successful land — the bloom's one
        /// resolved artifact tree.
        new_head: Digest,
    },
    /// Drive the bloom's git-side integration (ADR-0152 §Resolution drives
    /// integration): emitted by the [`Fact::Integrate`](crate::Fact::Integrate) that completes the claim
    /// set — every member now carries a resolution — so the host integrate
    /// reactor folds each claim's candidate tree onto the bloom's integration
    /// branch in member order and admits the [`Fact::Resolve`](crate::Fact::Resolve) whose
    /// `DispatchLand` the land reactor then consumes. A snapshot-inert outbox
    /// effect like [`Decision::DispatchAttempt`], appended so the prior
    /// decisions' wire discriminants are unchanged.
    DispatchIntegration {
        /// The bloom whose members all carry claims.
        bloom: BloomId,
        /// The sealed base the integration branch bootstraps at.
        base: Digest,
        /// Every member's claimed candidate tree, in member order — the fold
        /// sequence, and the resolve's integration lineage.
        candidates: Vec<Digest>,
    },
    /// Record (or clear) the folded integration held on the bloom while its
    /// aggregate review runs (ADR-0153): a verified [`Fact::Resolve`](crate::Fact::Resolve) sets it,
    /// a failing review verdict clears it (the fold is stale once a member's
    /// claim is revoked). Appended so the prior decisions' wire discriminants
    /// are unchanged.
    RecordIntegration {
        /// The bloom the fold is held on.
        bloom: BloomId,
        /// The fold to hold, or `None` to clear a stale one.
        integration: Option<FoldedIntegration>,
    },
    /// Record the bloom's consumed aggregate-review verdict count — the
    /// two-pass ceiling's cursor (ADR-0153).
    RecordAggregateRoll {
        /// The reviewed bloom.
        bloom: BloomId,
        /// The verdicts consumed so far, this one included.
        rolls: u32,
    },
    /// Revoke a member's resolution claim (ADR-0153): a failing aggregate
    /// review re-opens every implicated member, and a bloom with a revoked
    /// claim cannot resolve until the member re-verifies and re-integrates.
    RevokeResolution {
        /// The bloom the claim is revoked on.
        bloom: BloomId,
        /// The re-opened member.
        workpiece: WorkpieceId,
    },
    /// Dispatch the whole-bloom aggregate review against the integrated head
    /// (ADR-0153) — the `review.critic` lane run once per bloom, judging the
    /// whole diff against the sealed intent. A snapshot-inert outbox effect
    /// like [`Decision::DispatchAttempt`]; the host wraps the transformation
    /// in a work order under a bloom-level order record.
    DispatchAggregateReview {
        /// The reviewed bloom.
        bloom: BloomId,
        /// The review lane transformation: `inputs[0]` is the integrated tree
        /// digest the returned evidence binds, `checkout` the landable head
        /// commit the critic checks out.
        transformation: Transformation,
        /// Which review pass this dispatches (`1` the full review, `2` the
        /// delta-confirm against the frozen finding set).
        roll: u32,
    },
    /// Record (or clear) the bloom-scope park (ADR-0153): raised when the
    /// delta-confirm still fails at the two-pass ceiling, holding the failing
    /// review's record artifact as a pending question (ADR-0151's hold
    /// vocabulary at bloom scope). Recording inserts the question into the
    /// bloom's open holds; clearing drops only the marker — the hold's release
    /// is [`Decision::ReleaseHold`]'s, emitted alongside by the adopting
    /// answer. Appended so the prior decisions' wire discriminants are
    /// unchanged.
    RecordReviewPark {
        /// The parked bloom.
        bloom: BloomId,
        /// The parked question digest, or `None` to clear on adoption.
        question: Option<Digest>,
    },
}
