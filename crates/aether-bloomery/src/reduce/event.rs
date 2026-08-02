//! The reducer's input vocabulary: the closed set of admitted facts and the
//! idempotency key each arrives under (ADR-0149 §The control core).

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
use crate::values::{BloomSpec, CandidateRef, Evidence, ResolutionClaim, Statement};

/// An admitted fact plus its idempotency key (ADR-0149 §The control core).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Event {
    /// The idempotency key — a replayed key reduces to [`Outcome::Duplicate`](crate::Outcome::Duplicate).
    pub idempotency_key: IdempotencyKey,
    /// The fact.
    pub fact: Fact,
}

/// The closed set of admitted facts (ADR-0149 §The line: a closed enum, not a
/// workflow language).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Fact {
    /// Seal a draft into an active bloom.
    Seal(BloomSpec),
    /// Supersede a predecessor with a successor that atomically inherits its
    /// claims.
    Supersede {
        /// The bloom being superseded.
        predecessor: BloomId,
        /// The successor spec sealing on the new membership/base/policy.
        successor: BloomSpec,
    },
    /// Integrate one member's resolved candidate, with evidence bound to it.
    Integrate {
        /// The bloom being integrated into.
        bloom: BloomId,
        /// The per-member resolution claim.
        claim: ResolutionClaim,
    },
    /// Admit non-integrating evidence (a study record, verification result, or
    /// review finding) into a bloom's evidence log (ADR-0151). The
    /// [`EvidenceKind`](crate::EvidenceKind) discriminant separates the classes; admission binds the
    /// evidence to its own subject and never advances a member toward
    /// resolution. A resolving [`ResolutionClaim`] never enters here — that is
    /// [`Fact::Integrate`]'s terminal.
    AdmitEvidence {
        /// The bloom the evidence is admitted against.
        bloom: BloomId,
        /// The evidence, bound to the exact subject digest it names.
        evidence: Evidence,
    },
    /// Resolve a bloom into its one artifact, once every member is integrated.
    Resolve {
        /// The bloom being resolved.
        bloom: BloomId,
        /// The final integrated tree digest.
        tree: Digest,
        /// The landable head commit's digest (distinct from `tree`), carried
        /// from the integrate outcome so the emitted `DispatchLand` swaps
        /// mainline onto a commit rather than the artifact tree.
        head: Digest,
        /// The integration lineage.
        lineage: Vec<Digest>,
    },
    /// Land a resolved bloom by compare-and-swap against its sealed base.
    ///
    /// The base is the bloom's own `spec.base()` — the only base a V1 bloom may
    /// land on (rebasing is forbidden), so it is not a caller argument: a
    /// caller-supplied base could name a moved head and land evidence gathered
    /// against the sealed base onto it (ADR-0149 §The bloom).
    Land {
        /// The bloom being landed.
        bloom: BloomId,
        /// The new mainline head.
        new_head: Digest,
    },
    /// Adopt an answer to a parked question, releasing its hold and
    /// re-dispatching the held stage (ADR-0151). The answer is a native
    /// [`Statement`] whose `parents` name the held question's exact digest —
    /// the observation→intent adoption ADR-0149 §The boundary defines, reused:
    /// an answer is intent, not evidence, so it enters here and not through
    /// [`Fact::AdmitEvidence`]. The reducer admits it only when it is
    /// instruction-capable (an author signature) and its parents name an open
    /// hold; the cryptographic `verify_authority` gate is the host answer
    /// route's, before admission (the reducer holds no key material), mirroring
    /// how the intake broker is the trust gate for evidence the reducer only
    /// re-checks for binding.
    ///
    /// Appended to the closed [`Fact`] enum past ADR-0151's evidence-admission
    /// variant to realize the ADR's answer path ("releases the hold and
    /// re-dispatches the held stage") as its own admitted fact, distinct from
    /// the evidence door — appended, not inserted, so the wire discriminants of
    /// the prior facts are unchanged.
    AdoptAnswer {
        /// The bloom the parked question belongs to.
        bloom: BloomId,
        /// The adopting answer statement — instruction-capable, its parents
        /// naming the held question digest.
        answer: Statement,
    },
    /// A dispatched per-member attempt completed with evidence (ADR-0149 §The
    /// line, ADR-0153). Admitted when a nonce/digest-matched attempt result
    /// arrives from evidence intake (#3502) for `Construct`, a failing
    /// `Verify`, or the repair-only `Refine`. The reducer evaluates the stage's
    /// completion gate against `passed` and the member's cursor: a passing gate
    /// advances the cursor and dispatches the next stage (a passing `Refine`
    /// returns to `Verify` for the delta-confirm); a failing `Construct` or
    /// `Refine` re-dispatches the same stage while the `retry_budget` allows
    /// and wedges the member once it is exhausted; a failing `Verify` re-enters
    /// `Refine` under the repair ceiling. The terminal `Verify` stage's passing
    /// result integrates the member through [`Fact::Integrate`] instead — the
    /// intake is stage-aware and never routes a passing `Verify` result here.
    ///
    /// `passed` is the completion gate's outcome as the intake broker read it from
    /// the worker's verdict — the reducer owns the *advance* decision the gate
    /// gates (advance / retry / wedge), never delegating that to the host; the
    /// host only reports the raw pass/fail observation. Appended past
    /// [`Fact::AdoptAnswer`] so the prior facts' wire discriminants are unchanged.
    AttemptCompleted {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member workpiece whose attempt completed.
        workpiece: WorkpieceId,
        /// The stage the completed attempt ran — must be the member's current
        /// cursor stage, or the completion is a stale/mismatched result.
        stage: StageId,
        /// The completion gate's pass/fail outcome for this attempt.
        passed: bool,
        /// The evidence the attempt produced, bound to its subject. Recorded in
        /// the bloom's evidence log; the binding is enforced at the intake trust
        /// boundary before admission (#3502) and re-checkable there like a claim's.
        evidence: Evidence,
        /// The candidate the attempt captured (ADR-0152) — the host records it
        /// after a model-lane run commits its work; absent on mechanical lanes
        /// and runs that produced nothing. Adopted onto the member's cursor only
        /// on a passing completion.
        candidate: Option<CandidateRef>,
    },
    /// A dispatched whole-bloom aggregate review completed with evidence
    /// (ADR-0153). Admitted when the review the fold dispatched returns a
    /// verdict against the integrated head: a passing one resolves the bloom
    /// from its held [`FoldedIntegration`](crate::FoldedIntegration); a failing one routes every
    /// implicated member back into `Refine` (revoking its claim — the bloom
    /// cannot resolve while any member is re-open) until the two-pass ceiling
    /// parks the bloom to the owner. Appended past [`Fact::AttemptCompleted`]
    /// so the prior facts' wire discriminants are unchanged.
    AggregateReviewCompleted {
        /// The reviewed bloom.
        bloom: BloomId,
        /// The review gate's pass/fail verdict.
        passed: bool,
        /// The review evidence, bound to the integrated tree it judged — the
        /// reducer refuses a verdict whose subject is not the held fold's
        /// tree, so a stale verdict cannot act on a newer integration.
        evidence: Evidence,
        /// The members owning the frozen findings a failing verdict routes to
        /// (ADR-0153 §Findings freeze) — each re-enters `Refine` once. Empty
        /// on a passing verdict; a *failing* verdict with an empty implication
        /// routes to every member — the host admits verdicts without
        /// membership knowledge, and over-routing is the fail-closed
        /// direction. The findings decomposition narrows it where ownership
        /// is parsed.
        implicated: Vec<WorkpieceId>,
    },
}
