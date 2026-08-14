//! The reducer's result vocabulary: what one event resolved to, paired with
//! the ordered effects it decided.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{
    AdmitEvidenceError, AdoptAnswerError, AggregateReviewError, AggregateReviewFault, AggregateVerifyError,
    AttemptCompletedError, Decision, FoldConflictError, GrantAttemptsError, IntegrateError, LandError,
    LandingRejectedError, OrphanClaimReleaseError, ResolveError, SealError, SupersedeError, VerifyFailedError,
};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{LandingReceipt, OrphanClaimReleaseCompletion, ResolvedBloom, VerifyFailureSet};

/// The result of reducing one event: an outcome plus the ordered effects that
/// enter the transactional outbox.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Decisions {
    /// What the event resolved to.
    pub outcome: Outcome,
    /// The ordered effects to apply — empty when the outcome is a rejection
    /// or a duplicate.
    pub effects: Vec<Decision>,
}

impl Decisions {
    pub(super) fn rejected(outcome: Outcome) -> Self {
        Self { outcome, effects: Vec::new() }
    }
}

/// The closed set of event outcomes.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Outcome {
    /// The idempotency key was already applied — no-op.
    Duplicate,
    /// A bloom sealed.
    Sealed(BloomId),
    /// A seal was refused, naming the violated admission rule.
    SealRejected(SealError),
    /// A predecessor was superseded by a successor.
    Superseded {
        /// The superseded predecessor.
        predecessor: BloomId,
        /// The successor.
        successor: BloomId,
    },
    /// A supersession was refused.
    SupersedeRejected(SupersedeError),
    /// A member's candidate integrated.
    Integrated {
        /// The bloom integrated into.
        bloom: BloomId,
        /// The integrated workpiece.
        workpiece: WorkpieceId,
    },
    /// An integration was refused.
    IntegrateRejected(IntegrateError),
    /// Non-integrating evidence was admitted into a bloom's evidence log.
    EvidenceAdmitted {
        /// The bloom the evidence was admitted against.
        bloom: BloomId,
        /// The exact digest the admitted evidence attests to.
        subject: Digest,
    },
    /// An evidence admission was refused.
    AdmitEvidenceRejected(AdmitEvidenceError),
    /// A bloom resolved into its one artifact.
    Resolved(ResolvedBloom),
    /// A resolve was refused.
    ResolveRejected(ResolveError),
    /// A bloom landed.
    Landed(LandingReceipt),
    /// A land was refused, naming why.
    LandRejected(LandError),
    /// An answer was adopted: its held question's hold was released and the
    /// held stage re-dispatched (ADR-0151). Appended, so the prior outcomes'
    /// wire discriminants are unchanged.
    AnswerAdopted {
        /// The bloom the released question belonged to.
        bloom: BloomId,
        /// The released question's digest — the exact digest the answer adopted.
        question: Digest,
    },
    /// An answer adoption was refused.
    AdoptAnswerRejected(AdoptAnswerError),
    /// A passing attempt advanced the member to its next stage, dispatching it.
    /// Appended, so the prior outcomes' wire discriminants are unchanged.
    AttemptAdvanced {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The advanced member.
        workpiece: WorkpieceId,
        /// The stage that passed.
        from: StageId,
        /// The stage the member advanced to and dispatched.
        to: StageId,
    },
    /// A failing attempt re-dispatched the same stage within its retry budget.
    AttemptRetried {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The retried member.
        workpiece: WorkpieceId,
        /// The stage re-dispatched.
        stage: StageId,
        /// The attempt count after the re-dispatch (≤ the stage's retry budget).
        attempt: u32,
    },
    /// A failing attempt exhausted its stage's retry budget: the member wedged and
    /// stops dispatching (a supersession is the escape). No further attempt is
    /// dispatched — the bloom cannot resolve until the member is superseded.
    AttemptWedged {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The wedged member.
        workpiece: WorkpieceId,
        /// The stage that exhausted its retry budget.
        stage: StageId,
        /// The verifier identities in the terminal verdict that had already
        /// failed for this member. Empty for every non-Verify wedge.
        repeated_verifiers: VerifyFailureSet,
    },
    /// An attempt completion was refused (unknown bloom, non-member, or a stage
    /// that is not the member's current cursor).
    AttemptCompletedRejected(AttemptCompletedError),
    /// A failing terminal Verify routed the member back into Refine
    /// (ADR-0153) — the findings-directed repair re-entry that replaces
    /// re-running the mechanical gate on an unchanged candidate. Appended so
    /// the prior outcomes' wire discriminants are unchanged.
    RefineReentered {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The re-entered member.
        workpiece: WorkpieceId,
        /// The count of repeat repair rolls spent. A wholly novel failure set
        /// re-enters with zero rolls; any repeated identity spends exactly one.
        rolls: u32,
    },
    /// A verified integration fold dispatched the whole-bloom aggregate review
    /// (ADR-0153) — every member's claim checked out and the review lane now
    /// judges the integrated head. Appended so the prior outcomes' wire
    /// discriminants are unchanged, like every variant below.
    AggregateReviewDispatched {
        /// The bloom under review.
        bloom: BloomId,
        /// Which review pass was dispatched (`1` the full review, `2` the
        /// delta-confirm).
        roll: u32,
    },
    /// A failing aggregate review routed its implicated members back into
    /// Refine (ADR-0153 §Findings freeze): each claim is revoked and the bloom
    /// cannot resolve until every re-opened member re-verifies and
    /// re-integrates, after which the re-fold dispatches the delta-confirm.
    AggregateReviewReentered {
        /// The reviewed bloom.
        bloom: BloomId,
        /// The re-opened members, in the verdict's order.
        members: Vec<WorkpieceId>,
        /// The aggregate-review verdicts consumed, this one included.
        rolls: u32,
    },
    /// A failing aggregate review hit the two-pass ceiling: the bloom parks to
    /// the owner as a pending decision (ADR-0151's hold vocabulary at bloom
    /// scope) — the machine never buys a third roll. The owner resolves it by
    /// adopting an answer that names the parked question (re-arming the review
    /// cycle), superseding, or abandoning.
    AggregateReviewParked {
        /// The parked bloom.
        bloom: BloomId,
        /// The verdicts consumed, this one included.
        rolls: u32,
        /// The parked question's digest — the failing review's record
        /// artifact, held open until an adopting answer names it.
        question: Digest,
    },
    /// An aggregate-review completion was refused.
    AggregateReviewRejected(AggregateReviewError),
    /// An observation moved mainline onto the repository's live head (#4667).
    MainlineAdvanced {
        /// The head mainline moved off.
        from: Digest,
        /// The observed head mainline moved onto.
        to: Digest,
    },
    /// An observation named the head mainline already sits at. The steady state
    /// of a host that re-observes on a cadence, so it is a plain no-op rather
    /// than a refusal — nothing is wrong, there is simply nothing to move.
    MainlineUnchanged(Digest),
    /// The repository is ahead of mainline, which may not follow yet because a
    /// bloom is in flight (#4709).
    ///
    /// Not a refusal: the observation is recorded, and a supersession that
    /// rebases onto this head is what lets mainline catch up. Refusing outright
    /// is what left a wedged bloom pinning mainline forever, since a wedge never
    /// leaves flight on its own.
    MainlineHeld {
        /// The head the repository is at.
        head: Digest,
        /// The in-flight bloom mainline is waiting on.
        by: BloomId,
    },
    /// A complete claim set folded and dispatched the whole-bloom aggregate
    /// verify — the mechanical gate over the fold, ahead of the critic.
    AggregateVerifyDispatched {
        /// The bloom under verification.
        bloom: BloomId,
        /// Which verify pass was dispatched.
        roll: u32,
    },
    /// A passing aggregate verify handed the same fold to the aggregate review:
    /// the fold builds, so it is now worth judging.
    AggregateVerifyPassed {
        /// The verified bloom.
        bloom: BloomId,
        /// The verify verdicts consumed, this one included.
        rolls: u32,
    },
    /// A failing aggregate verify re-opened every member into Refine: the fold
    /// does not build, and the failure belongs to the combination rather than
    /// to any one member that passed on its own.
    AggregateVerifyReentered {
        /// The verified bloom.
        bloom: BloomId,
        /// The re-opened members, in sealed membership order.
        members: Vec<WorkpieceId>,
        /// The verify verdicts consumed, this one included.
        rolls: u32,
    },
    /// A failing aggregate verify spent the stage's budget: the bloom parks to
    /// the owner rather than re-folding a combination that has not built yet.
    AggregateVerifyParked {
        /// The parked bloom.
        bloom: BloomId,
        /// The verdicts consumed, this one included.
        rolls: u32,
        /// The parked question's digest — the failing verify's record
        /// artifact, held open until an adopting answer names it.
        question: Digest,
    },
    /// An aggregate-verify completion was refused.
    AggregateVerifyRejected(AggregateVerifyError),
    /// A refused landing un-resolved the bloom and re-opened its members: the
    /// landing gate judged the fold against a mainline no gate inside the loop
    /// sees, so the line reopens to answer it.
    LandingReentered {
        /// The bloom returned to the working state.
        bloom: BloomId,
        /// The re-opened members, in sealed membership order.
        members: Vec<WorkpieceId>,
        /// The landing attempts consumed, this one included.
        rolls: u32,
    },
    /// A refused landing spent the `Land` budget: the bloom parks to the owner
    /// rather than proposing a head its gate keeps refusing.
    LandingParked {
        /// The parked bloom.
        bloom: BloomId,
        /// The landing attempts consumed, this one included.
        rolls: u32,
        /// The parked question's digest — the rejection's record artifact.
        question: Digest,
    },
    /// A landing rejection was refused.
    LandingRejectedRefused(LandingRejectedError),
    /// A wedged member was handed back attempts and re-dispatched on the bloom
    /// it already belongs to (#4708) — no new bloom, no field of the spec
    /// altered, and the candidate it had already built carried forward.
    AttemptsGranted {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The resumed member.
        workpiece: WorkpieceId,
        /// The stage the member resumes at: the wedged stage itself, or `Refine`
        /// when the wedge was a spent repair ceiling at `Verify` — re-running the
        /// mechanical gate on an unchanged candidate cannot change its verdict.
        resumes_at: StageId,
        /// How many dispatched attempts the member may now spend before it
        /// wedges again.
        attempts: u32,
    },
    /// An attempt grant was refused.
    GrantAttemptsRejected(GrantAttemptsError),
    /// A typed member-Verify failure was refused.
    VerifyFailedRejected(VerifyFailedError),
    /// An authorized orphan-claim release was admitted and its source effect
    /// enqueued (ADR-0179). A repeat of a request id already on record resolves
    /// here too, carrying no effects — the recorded state is what the status
    /// route reads, and re-enqueuing would release twice.
    OrphanClaimReleaseRequested {
        /// The request digest — the handle the status route reads by.
        request: Digest,
    },
    /// An authorized release reached its terminal result.
    OrphanClaimReleaseCompleted {
        /// The completed request.
        request: Digest,
        /// Which terminal the source reached.
        completion: OrphanClaimReleaseCompletion,
    },
    /// An orphan-claim release request or completion was refused.
    OrphanClaimReleaseRejected(OrphanClaimReleaseError),
    /// An aggregate review could not run and the same held fold was redispatched
    /// (ADR-0176) — a bounded retry of the *review*, not a repair of anything.
    ///
    /// Nothing about the bloom's work moved: the fold is still held, every claim
    /// still stands, and no member left its cursor. Appended so the prior
    /// outcomes' wire discriminants are unchanged, like the variant below.
    AggregateReviewExecutorFaulted {
        /// The bloom whose review could not run.
        bloom: BloomId,
        /// The fault series this fault produced, keyed to the held fold.
        fault: AggregateReviewFault,
        /// The sealed `AggregateReview` retry budget the series is bounded by.
        budget: u32,
    },
    /// An aggregate review's executor faults reached the sealed budget: the
    /// bloom carries a terminal executor-fault wedge and dispatches nothing
    /// further (ADR-0176).
    ///
    /// Not an ADR-0151 park — there is no pending product decision to adopt, so
    /// no question is raised and no answer releases it. Recovery is an explicit
    /// successor after an operator repairs the environment.
    AggregateReviewExecutorWedged {
        /// The wedged bloom.
        bloom: BloomId,
        /// The terminal fault series.
        fault: AggregateReviewFault,
        /// The sealed budget the series exhausted.
        budget: u32,
    },
    /// A member advanced onto a tree the bloom already holds a green verify
    /// verdict for, so its terminal `Verify` passed by identity and the member
    /// integrated on the recorded verdict (#4891).
    ///
    /// Takes the place of the [`Outcome::AttemptAdvanced`] the same completion
    /// would otherwise have returned: the cursor still lands on `Verify`, but
    /// nothing is dispatched against it, so the member is integrated by the time
    /// this outcome is returned. The live case is a repair lap that changed
    /// nothing the tree records — an amended commit message leaves the candidate
    /// its previous verify already proved.
    VerifyReused {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member that passed by identity.
        workpiece: WorkpieceId,
        /// The reused verdict's artifact digest.
        proof: Digest,
    },
    /// A fold arrived on a tree the bloom already holds a green verify verdict
    /// for, so the aggregate verify passed by identity and the critic got the
    /// fold without the mechanical gate running again (#4891).
    ///
    /// The sibling of [`Outcome::AggregateVerifyDispatched`], returned by the
    /// same resolve: a single-member fold is byte-identical to the candidate its
    /// member verified, and re-proving it buys a verdict the journal already
    /// holds. Distinct from [`Outcome::AggregateVerifyPassed`] because no verdict
    /// arrived — nothing was dispatched to return one. Appended so the prior
    /// outcomes' wire discriminants are unchanged.
    AggregateVerifyReused {
        /// The bloom whose fold passed by identity.
        bloom: BloomId,
        /// The verify verdicts consumed, this pass included — counted like a
        /// dispatched one, so the stage's budget reads the same either way.
        rolls: u32,
        /// The reused verdict's artifact digest, so the caller can name the
        /// proof this pass stood on without reading the record back.
        proof: Digest,
    },
    /// A fold collision dispatched the member's `Reconcile` stage (ADR-0189).
    /// Appended so the prior outcomes' wire discriminants are unchanged.
    FoldConflictDispatched {
        /// The bloom whose fold collided.
        bloom: BloomId,
        /// The later-folding member now reconciling.
        workpiece: WorkpieceId,
    },
    /// A fold-conflict admission was refused.
    FoldConflictRejected(FoldConflictError),
    /// An observation named a head that is an ancestor of, or unrelated to,
    /// the current mainline correspondence (#4938). Appended so every prior
    /// outcome keeps its wire discriminant.
    ///
    /// Not a hold: the head is not something mainline should follow later.
    /// Recording it as `observed` would poison the only base a supersession
    /// may rebase onto, so the refusal carries no effects.
    MainlineDiverged {
        /// The observed head that would have moved mainline backward or sideways.
        head: Digest,
        /// The mainline the observation was classified against.
        mainline: Digest,
    },
}
