//! The reducer's result vocabulary: what one event resolved to, paired with
//! the ordered effects it decided.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{
    AdjudicationError, AdmitEvidenceError, AdoptAnswerError, AggregateReviewError, AggregateReviewFault,
    AggregateVerifyError, AttemptCompletedError, BaseReverifyError, Decision, FoldConflictError, GrantAttemptsError,
    HostFaultError, IntegrateError, LandError, LandingRejectedError, LeaseObservationError, MemberExecutorFaultError,
    NarrowCompositionError, OperatorHoldError, OperatorRepairError, OrphanClaimReleaseError, ResolveError, SealError,
    SpliceError, SupersedeError, SuppressionDispositionError, SurfaceRequestedError, VerifyFailedError, WithdrawError,
};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{
    EvictedHolder, LandingReceipt, OrphanClaimReleaseCompletion, ResolvedBloom, SpendQuiesce, VerifyFailureSet,
};

/// The result of reducing one event: an outcome plus the ordered effects that
/// enter the transactional outbox.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Decisions {
    /// What the event resolved to.
    pub outcome: Outcome,
    /// The ordered effects to apply.
    ///
    /// Empty for a duplicate, and for a rejection that has nothing to say. An
    /// operator-visible boundary's rejection carries one
    /// [`Decision::RecordRefusal`] (ADR-0206) — a rejected event is journaled
    /// with its effects, so the refusal folds on replay exactly as it did at
    /// admission. Nothing else rides a rejection: no membership claim, no
    /// outbox row.
    pub effects: Vec<Decision>,
}

impl Decisions {
    pub(super) fn rejected(outcome: Outcome) -> Self {
        Self { outcome, effects: Vec::new() }
    }
}

/// The closed set of event outcomes.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
        /// failed for this member. Empty for every non-Verify wedge, and for a
        /// `Verify` wedge reached on verdicts that named no verifier at all —
        /// an empty set here is how a gate that never answered reads.
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
    /// A passing aggregate verify whose sibling review has not returned yet:
    /// the fold builds, and the bloom waits on the critic judging it in
    /// parallel. Nothing is dispatched from here — both gates went out
    /// together.
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
        /// The stage the member resumes at, which names the granted axis with
        /// the count below: the wedged stage itself on a `MACHINERY` grant
        /// (and on a non-`Verify` `WORK` grant), or `Refine` when a `WORK`
        /// wedge at `Verify` spent the repair ceiling — re-running the
        /// mechanical gate on an unchanged candidate cannot change its verdict.
        resumes_at: StageId,
        /// How many dispatched attempts the member may now spend on that axis
        /// before it wedges again. A `MACHINERY` grant spends this against the
        /// independent host-fault series; a `WORK` grant spends it against
        /// ordinary attempts or Verify repair rolls.
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
    /// An observation named a head that is a strict ancestor of the current
    /// mainline correspondence (#4938). Appended so every prior outcome
    /// keeps its wire discriminant.
    ///
    /// Not a hold: the head is not something mainline should follow later.
    /// Recording it as `observed` would poison the only base a supersession
    /// may rebase onto, so the refusal carries no effects. A rewritten
    /// live ref does not land here — the host follows it as an ordinary
    /// [`Outcome::MainlineAdvanced`] (or Held).
    MainlineDiverged {
        /// The observed head that would have moved mainline backward or sideways.
        head: Digest,
        /// The mainline the observation was classified against.
        mainline: Digest,
    },
    /// A refused composition dispatched the weave repair (ADR-0191 §5) — the
    /// composition workpiece's own `Refine`, against the composed tree that was
    /// refused. No member's claim is revoked and no member is dispatched:
    /// members are immutable after review, and a defect discovered in the
    /// composition belongs to the composition. Appended so every prior outcome
    /// keeps its wire discriminant, like the two below.
    CompositionRewoven {
        /// The bloom whose composition is repairing.
        bloom: BloomId,
        /// The stage whose verdict refused the composition — its `Verify` (the
        /// composite gate run), its `Review` (the intent-preservation judgment),
        /// or `Land` when the landing gate refused the weave.
        refused_at: StageId,
        /// The weave-repair attempt this dispatch is, against the composition's
        /// own retry budget.
        attempt: u32,
    },
    /// The composition spent its weave-repair budget and stopped (ADR-0191 §5):
    /// the wedge an operator reads, in the existing wedge vocabulary. Recovery
    /// is the ordinary one — an attempt grant against the composition, or a
    /// successor — never a member re-entry.
    CompositionWedged {
        /// The wedged bloom.
        bloom: BloomId,
        /// The stage whose verdict spent the last of the budget.
        refused_at: StageId,
        /// The finding artifact the wedge carries.
        question: Digest,
    },
    /// A weave repair returned and the composition advanced back to its
    /// `Verify`, re-dispatching the composite gate run over the re-woven tree.
    CompositionRepaired {
        /// The bloom whose composition repaired.
        bloom: BloomId,
        /// The re-woven tree the gates now run over.
        tree: Digest,
    },
    /// An operator closed a bloom's named composition findings (#4957).
    /// Appended so every prior outcome keeps its wire discriminant, like the
    /// three below.
    FindingsAdjudicated {
        /// The bloom whose findings were closed.
        bloom: BloomId,
        /// The findings this adjudication closed, in the order it named them.
        closed: Vec<Digest>,
        /// Whether closing them let the composition proceed to its landing —
        /// `false` when the bloom holds no weave to land, or when the weave it
        /// holds has no green aggregate-verify proof yet (#5104). The first is
        /// the state a landing-refused composition wedges in, where the
        /// operator's next move is a repair rather than a waiver. The second
        /// leaves the ordinary refine cycle to finish proving the current head;
        /// landing dispatches once that proof is green.
        proceeds_to_landing: bool,
    },
    /// An operator adjudication was refused.
    AdjudicationRejected(AdjudicationError),
    /// An operator-supplied repair candidate re-entered the workpiece's line at
    /// `Verify` (#4957) — the ordinary gate run over someone else's lap.
    OperatorRepairAccepted {
        /// The bloom the repaired workpiece belongs to.
        bloom: BloomId,
        /// The repaired workpiece — a member, or the composition.
        workpiece: WorkpieceId,
        /// The candidate tree the gates now run over.
        candidate: Digest,
    },
    /// An operator-supplied repair was refused.
    OperatorRepairRejected(OperatorRepairError),
    /// A bloom's dispatch was put on the operator brake (#4976). Everything
    /// already in flight keeps running and keeps journaling; nothing new goes
    /// out until it is released — member laps and the two aggregate gates
    /// alike (#5100).
    BloomHeld {
        /// The frozen bloom.
        bloom: BloomId,
    },
    /// The operator brake came off (#4976), and the dispatches the hold
    /// swallowed were re-derived from the record and sent.
    BloomReleased {
        /// The bloom let go.
        bloom: BloomId,
        /// The workpieces dispatched on the way out, in workpiece order. Empty
        /// when the hold swallowed no member lap — a bloom held and released
        /// while every lap it had was still running owes no member dispatch.
        /// Owed aggregate gates ride as ordinary
        /// [`Decision::DispatchAggregateVerify`] /
        /// [`Decision::DispatchAggregateReview`] effects beside this outcome;
        /// they are not workpieces and do not appear here.
        dispatched: Vec<WorkpieceId>,
    },
    /// An operator hold or release was refused.
    OperatorHoldRejected(OperatorHoldError),
    /// The seal door found two members declaring overlapping surfaces (#4931).
    ///
    /// Named as its own outcome rather than folded into
    /// [`Outcome::Sealed`], because it is a second reading of the same
    /// admission and not the admission's result: the seal proceeds either way,
    /// and a bloom whose members all declare disjoint surfaces produces none of
    /// these at all. Appended so every prior outcome keeps its wire
    /// discriminant.
    SurfaceOverlap {
        /// The two members whose declared surfaces intersect.
        members: Vec<WorkpieceId>,
        /// The globs both declared surfaces permit.
        intersection: Vec<String>,
    },
    /// The seal door closed because the window's measured spend is at or over
    /// a sealed ceiling (ADR-0192). Appended so every prior outcome keeps its
    /// wire discriminant.
    ///
    /// Not a [`Outcome::SealRejected`]: every other seal refusal names
    /// something wrong with the draft, and a spend refusal names something
    /// true about the fleet. The crossing is recorded as
    /// [`Decision::RecordSpendQuiesce`]
    /// so `/view` can show the marker rather than a silent refusal to seal.
    SealQuiesced(SpendQuiesce),
    /// A member Verify could not run because the host is missing a gate tool
    /// (#5020). The member stays at Verify; nothing is charged to its budget
    /// and Refine is not dispatched.
    VerifyHostFaultHeld {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member held on the host fault.
        workpiece: WorkpieceId,
    },
    /// A host-fault hold was cleared and Verify re-dispatched (#5020).
    HostFaultResumed {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member whose Verify is running again.
        workpiece: WorkpieceId,
    },
    /// A host-fault hold or resume was refused.
    HostFaultRejected(HostFaultError),
    /// The host assembled a dependent's multi-tip splice and Construct
    /// dispatched on the merged head (ADR-0196 G2). Appended so every prior
    /// outcome keeps its wire discriminant.
    SpliceAssembled {
        /// The bloom the dependent belongs to.
        bloom: BloomId,
        /// The dependent now constructing on the assembled head.
        workpiece: WorkpieceId,
    },
    /// A splice-assembly admission was refused.
    SpliceRejected(SpliceError),
    /// A member stage could not run and the same artifact was redispatched
    /// (ADR-0195) — a bounded retry of the *gate*, not a repair of anything.
    ///
    /// Nothing about the member's work moved: the candidate still stands, and
    /// neither `attempts` nor `repair_rolls` advanced. Appended so the prior
    /// outcomes' wire discriminants are unchanged, like the variant below.
    MachineryRetried {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The retried member.
        workpiece: WorkpieceId,
        /// The stage re-dispatched.
        stage: StageId,
        /// Machinery faults this stage has taken, this one included.
        rolls: u32,
        /// The sealed stage retry budget the series is bounded by.
        budget: u32,
    },
    /// A member stage's executor faults reached the sealed budget: the member
    /// carries a terminal machinery-cause wedge and dispatches nothing further
    /// (ADR-0195).
    ///
    /// Recovery is an operator door after the host is repaired — not a Refine
    /// lap, and not another poll that quietly buys an attempt.
    MachineryWedged {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The wedged member.
        workpiece: WorkpieceId,
        /// The stage that exhausted its machinery budget.
        stage: StageId,
        /// Machinery faults this stage took, the terminal one included.
        rolls: u32,
        /// The sealed budget the series exhausted.
        budget: u32,
    },
    /// A member machinery-fault admission was refused.
    MemberExecutorFaultRejected(MemberExecutorFaultError),
    /// The integrate fold refused at a named guard (ADR-0206). Appended so
    /// every prior outcome keeps its wire discriminant.
    FoldRefused {
        /// The bloom whose fold refused.
        bloom: BloomId,
    },
    /// A fold-refusal admission was refused — no sealed bloom with that id.
    FoldRefusalRejected,
    /// A construct lane concluded without a candidate (#5292): the member parks
    /// rather than retrying, spending no attempt and no repair roll. Recovery
    /// is a different declared surface, not a grant. Appended so every prior
    /// outcome keeps its wire discriminant.
    AttemptParked {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The parked member.
        workpiece: WorkpieceId,
        /// The stage that declined — Construct, the only stage that parks this way.
        stage: StageId,
        /// The lane's evidence artifact — the diagnosis an operator reads.
        reason: Digest,
    },
    /// A member declined and asked for the surface its work requires
    /// (ADR-0207). Appended last so every prior outcome keeps its wire
    /// discriminant.
    SurfaceRequested {
        /// The bloom.
        bloom: BloomId,
        /// The member now awaiting a surface amendment.
        workpiece: WorkpieceId,
        /// The stage it declined at.
        stage: StageId,
        /// The request's content-addressed id — the digest the granting half's
        /// authorization names.
        request: Digest,
        /// How many times this member has requested in this bloom, including
        /// this one (ADR-0207 §Amendments are budgeted).
        requests: u32,
    },
    /// A surface-request admission was refused.
    SurfaceRequestRejected(SurfaceRequestedError),
    /// An operator withdrew members from a walking bloom (#5327). Appended
    /// last so every prior outcome keeps its wire discriminant.
    MembersWithdrawn {
        /// The bloom the members left.
        bloom: BloomId,
        /// Every member withdrawn by this act, the operator-named ones first
        /// and each cascaded dependent after, in sealed member order.
        withdrawn: Vec<WorkpieceId>,
        /// Whether that emptied the bloom, moving it to
        /// [`BloomStatus::Withdrawn`](crate::BloomStatus::Withdrawn).
        terminal: bool,
    },
    /// A withdrawal was refused. An operator-door refusal: the REST edge
    /// answers it `422`, so a script that only checks the status cannot read a
    /// refused withdrawal as an applied one.
    WithdrawRejected(WithdrawError),
    /// A passing aggregate review whose sibling verify has not returned yet.
    ///
    /// The two composite gates run against one fold at the same time, and a
    /// landing needs both, so a review that arrives first records its pass and
    /// the bloom waits. The verify's own arrival is what resolves it. Appended
    /// so every prior outcome keeps its wire discriminant.
    AggregateReviewPassed {
        /// The reviewed bloom.
        bloom: BloomId,
        /// The review verdicts consumed, this one included.
        rolls: u32,
    },
    /// A composite gate refused a fold whose weave repair is already dispatched.
    ///
    /// The two gates judge the same tree concurrently, so both can refuse it.
    /// The second refusal files its finding on the composition's channel and
    /// stops there: a second repair lap would double-spend the weave budget and
    /// put two lanes on one seam. Appended so every prior outcome keeps its wire
    /// discriminant.
    CompositionRepairInFlight {
        /// The bloom whose composition is already under repair.
        bloom: BloomId,
        /// Which gate filed this second refusal.
        refused_at: StageId,
    },
    /// A construct lane's observed write set moved the bloom's file-lease
    /// table (ADR-0204). Appended last so every prior outcome keeps its wire
    /// discriminant.
    LeasesObserved {
        /// The bloom whose lanes contend.
        bloom: BloomId,
        /// The observed member.
        workpiece: WorkpieceId,
        /// The paths this observation took a lease on, in path order. Empty
        /// when the member already held everything it was seen writing.
        acquired: Vec<String>,
        /// The later-canonical siblings this observation evicted, in member
        /// order, each with the path that took it. Each has its lane cancelled
        /// and re-dispatches once `workpiece` integrates.
        evicted: Vec<EvictedHolder>,
    },
    /// A lane-write observation was refused.
    LeaseObservationRejected(LeaseObservationError),
    /// A reviewer answered a member's standing suppression requests
    /// (ADR-0193). Appended last so every prior outcome keeps its wire
    /// discriminant.
    SuppressionAnswered {
        /// The bloom the answered member belongs to.
        bloom: BloomId,
        /// The answered member.
        workpiece: WorkpieceId,
        /// Whether the member re-opens at `Refine`. A grant lets the candidate
        /// stand; a denial spends a repair roll exactly as any other bounced
        /// lap does.
        reopened: bool,
    },
    /// A suppression disposition was refused.
    SuppressionRejected(SuppressionDispositionError),
    /// A `verify.base` dispatch was queued for an unproven sealed base
    /// (ADR-0200). Appended so every prior outcome keeps its wire discriminant.
    BaseVerifyQueued {
        /// The commit the queued run will check out.
        base: Digest,
    },
    /// A whole-workspace base verify passed, and the withheld member dispatches
    /// it was holding went out. Appended so every prior outcome keeps its wire
    /// discriminant.
    BaseProven {
        /// The commit the verify ran at.
        base: Digest,
        /// The tree it peeled to.
        tree: Digest,
        /// The blooms whose deferred entry dispatches this verdict released.
        released: Vec<BloomId>,
    },
    /// A whole-workspace base verify failed. Member entry stays withheld.
    /// Appended so every prior outcome keeps its wire discriminant.
    BaseRefused {
        /// The commit the verify ran at.
        base: Digest,
        /// The tree it peeled to.
        tree: Digest,
        /// The verifier identities that failed together.
        failed: VerifyFailureSet,
    },
    /// A failing fold was attributed to the candidates that account for it and
    /// the synthetic subject that repairs their coexistence was minted
    /// (ADR-0210). Appended so every prior outcome keeps its wire discriminant.
    CompositionNarrowed {
        /// The bloom whose fold refused.
        bloom: BloomId,
        /// The minted subject.
        workpiece: WorkpieceId,
        /// Its parents, in canonical id order. The arity is this list's length.
        parents: Vec<WorkpieceId>,
        /// The union of the parents' declared surfaces — what its repair may
        /// edit, recorded here because it is derived rather than sealed and so
        /// has to be readable after the fact.
        bound: Vec<String>,
        /// Which repair lap this is.
        attempt: u32,
    },
    /// A second verdict refused a fold a narrowed composition is already
    /// repairing. The verdict is filed; no second lane is bought.
    CompositionRepairAlreadyInFlight {
        /// The bloom.
        bloom: BloomId,
        /// The subject already repairing that tree.
        workpiece: WorkpieceId,
    },
    /// A narrowed composition exhausted its repair budget. Its parents' intents could not
    /// be made to coexist inside the laps the catalog allows.
    NarrowCompositionWedged {
        /// The bloom.
        bloom: BloomId,
        /// The wedged subject.
        workpiece: WorkpieceId,
        /// The verdict that spent the last lap.
        question: Digest,
    },
    /// An attribution of a failing fold was refused.
    NarrowCompositionRejected(NarrowCompositionError),
    /// **Retired**, and kept for the same reason
    /// [`Fact::SurfaceGranted`](super::Fact::SurfaceGranted) is: journaled
    /// [`Decisions`] blobs already carry it, so removing it stops those rows
    /// decoding. Kept in this declaration position so every prior outcome
    /// keeps its wire discriminant.
    ///
    /// A member's surface request was granted by the machinery and the member
    /// re-entered the line. Nothing produces it now.
    SurfaceGranted {
        /// The bloom.
        bloom: BloomId,
        /// The member whose surface grew.
        workpiece: WorkpieceId,
        /// The successor revision it dispatched under.
        revision: Digest,
        /// The globs the grant added.
        added: Vec<String>,
    },
    /// A surface grant was refused. **Retired** as an admission outcome, and
    /// the answer the reducer now gives a retired
    /// [`Fact::SurfaceGranted`](super::Fact::SurfaceGranted) that reaches it:
    /// [`SurfaceRequestedError::GrantRetired`], with no effects.
    SurfaceGrantRejected(SurfaceRequestedError),
    /// A base re-verify was refused. Appended last so every prior outcome keeps
    /// its wire discriminant.
    BaseReverifyRejected(BaseReverifyError),
}

impl Outcome {
    /// Whether this outcome is a refused operator door (grant, adjudication,
    /// repair, or the brake).
    ///
    /// The REST edge answers those `422` rather than the `200` every other write
    /// route answers its outcome with, so the doors need one place that says
    /// which outcomes those are. It lives here because the vocabulary is this
    /// enum's: a route matching on variant names would drift the moment another
    /// operator refusal is appended.
    #[must_use]
    pub const fn is_refused_override(&self) -> bool {
        matches!(
            self,
            Self::GrantAttemptsRejected(_)
                | Self::AdjudicationRejected(_)
                | Self::OperatorRepairRejected(_)
                | Self::OperatorHoldRejected(_)
                | Self::WithdrawRejected(_)
                | Self::SuppressionRejected(_)
                | Self::BaseReverifyRejected(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::Digest;
    use crate::ids::{BloomId, StageId, WorkpieceId};
    use crate::persisted::{DECISIONS, PersistedSchemaError, decode_recorded_decisions};
    use crate::values::{
        AgentProfile, ConfigRegistry, ExecutionLimits, Harness, NetworkProfile, ReasoningEffort, ToolPolicy,
        Transformation, VerifyFailureSet,
    };
    use aether_data::wire::{Error as WireError, from_bytes, to_vec};

    // Tripwire: wire bytes of a DecisionsV1 whose effects are DispatchAttempt,
    // AdvanceStage with StageProgressV1, then RecordObservation — so the bool
    // #5330 added sits mid-stream. Decoding these as current Decisions returns
    // InvalidBool; the v1 upcast must keep the trailing observation intact.
    const V1_ROW: &[u8] = &[
        1, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 3,
        0, 0, 0, 11, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 2, 0, 0, 0, 119, 112, 3, 0, 0, 0, 19, 0, 0, 0, 99, 111, 110, 115, 116, 114, 117, 99, 116, 46, 105, 109,
        112, 108, 101, 109, 101, 110, 116, 0, 0, 0, 0, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 0, 0, 0, 0, 0, 16, 0, 0, 0, 105, 97, 109, 97, 47, 99, 111, 110, 115, 116, 114,
        117, 99, 116, 58, 49, 60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
        3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 0, 3, 0, 0, 0, 4, 0, 0, 0, 103, 114, 111, 107, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 119, 112, 3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 25, 0, 0,
        0, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
    ];

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn bloom() -> BloomId {
        BloomId(digest(1))
    }

    fn workpiece() -> WorkpieceId {
        WorkpieceId(String::from("wp"))
    }

    fn transformation() -> Transformation {
        Transformation {
            command: String::from("construct.implement"),
            inputs: Vec::new(),
            checkout: digest(2),
            diff_base: None,
            outputs: Vec::new(),
            image: String::from("iama/construct:1"),
            limits: ExecutionLimits { wall_clock_secs: 60 },
            network: NetworkProfile::None,
            description: None,
            model: None,
        }
    }

    fn profile() -> AgentProfile {
        AgentProfile {
            harness: Harness::Grok,
            model: String::from("grok"),
            effort: ReasoningEffort::Low,
            tools: ToolPolicy::None,
        }
    }

    fn dispatch() -> Decision {
        Decision::DispatchAttempt {
            bloom: bloom(),
            workpiece: workpiece(),
            stage: StageId::Construct,
            transformation: transformation(),
            scope_revision: digest(3),
            candidate: None,
            profile: profile(),
            configs: ConfigRegistry::default(),
        }
    }

    #[test]
    fn a_v1_row_carrying_a_dispatch_decodes_and_upcasts() {
        let stamped =
            decode_recorded_decisions(V1_ROW, Some(DECISIONS.upcast_digest(&DECISIONS.upcasts[0]).as_bytes()))
                .expect("stamped v1 decodes");
        let unstamped = decode_recorded_decisions(V1_ROW, None).expect("unstamped row is v1");
        assert_eq!(stamped, unstamped);

        match &stamped.effects[..] {
            [
                Decision::DispatchAttempt { .. },
                Decision::AdvanceStage { progress, .. },
                Decision::RecordObservation { head },
            ] => {
                assert!(!progress.reconcile_assembles_base, "pre-#5330 Reconcile never recorded an assembly");
                assert_eq!(*head, digest(9), "the trailing effect survives the missing mid-stream bool");
            }
            other => panic!("expected dispatch, advance, observation; got {other:?}"),
        }

        assert!(
            matches!(from_bytes::<Decisions>(V1_ROW), Err(WireError::InvalidBool(_))),
            "v1 bytes are not the current shape: the missing bool sits mid-stream"
        );
    }

    #[test]
    fn a_v2_row_decodes_as_current() {
        let recorded = Decisions {
            outcome: Outcome::Sealed(bloom()),
            effects: vec![
                dispatch(),
                Decision::AdvanceStage {
                    bloom: bloom(),
                    workpiece: workpiece(),
                    progress: crate::StageProgress {
                        stage: StageId::Construct,
                        attempts: 1,
                        candidate: None,
                        repair_rolls: 0,
                        seen_verify_failures: VerifyFailureSet::EMPTY,
                        fold_checkpoint: None,
                        fold_conflict_evidence: None,
                        reconcile_assembles_base: true,
                    },
                },
                Decision::RecordObservation { head: digest(9) },
            ],
        };
        let bytes = to_vec(&recorded).expect("v2 encodes");
        let decoded = decode_recorded_decisions(&bytes, Some(DECISIONS.current_digest().as_bytes()))
            .expect("current identity decodes");
        assert_eq!(decoded, recorded);
    }

    fn minted_v2_row() -> Decisions {
        Decisions {
            outcome: Outcome::Sealed(bloom()),
            effects: vec![
                dispatch(),
                Decision::AdvanceStage {
                    bloom: bloom(),
                    workpiece: workpiece(),
                    progress: crate::StageProgress {
                        stage: StageId::Construct,
                        attempts: 1,
                        candidate: None,
                        repair_rolls: 0,
                        seen_verify_failures: VerifyFailureSet::EMPTY,
                        fold_checkpoint: None,
                        fold_conflict_evidence: None,
                        reconcile_assembles_base: true,
                    },
                },
                Decision::RecordObservation { head: digest(9) },
            ],
        }
    }

    // Tripwire: a variant *inserted* rather than appended shifts every later
    // discriminant and the boot replay misreads the journal, which is the
    // #5338 abort class. These bytes are a `Decisions` whose effects include
    // a `DispatchAttempt` followed by at least one further effect, minted
    // once from the current shape.
    const V2_ROW: &[u8] = &[
        1, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 3,
        0, 0, 0, 11, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 2, 0, 0, 0, 119, 112, 3, 0, 0, 0, 19, 0, 0, 0, 99, 111, 110, 115, 116, 114, 117, 99, 116, 46, 105, 109,
        112, 108, 101, 109, 101, 110, 116, 0, 0, 0, 0, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 0, 0, 0, 0, 0, 16, 0, 0, 0, 105, 97, 109, 97, 47, 99, 111, 110, 115, 116, 114,
        117, 99, 116, 58, 49, 60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
        3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 0, 3, 0, 0, 0, 4, 0, 0, 0, 103, 114, 111, 107, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 119, 112, 3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 25, 0,
        0, 0, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
    ];

    #[test]
    fn appending_base_verify_variants_leaves_a_v2_row_decoding() {
        // Tripwire: a variant inserted rather than appended shifts every later
        // discriminant and the boot replay misreads the journal, which is the
        // #5338 abort class.
        let expected = minted_v2_row();
        let stamped = decode_recorded_decisions(V2_ROW, Some(DECISIONS.current_digest().as_bytes()))
            .expect("checked-in v2 decodes");
        assert_eq!(stamped, expected);
    }

    #[test]
    fn an_unknown_stamp_is_refused_by_name() {
        // A reshape without an upcast must refuse replay by the identities
        // involved, never silently invent a value.
        let found = Digest::from_bytes([0xcd; 32]);
        let error =
            decode_recorded_decisions(b"x", Some(found.as_bytes())).expect_err("an unknown identity must refuse");
        let text = format!("{error}");
        assert!(text.contains(&format!("no migration from schema `{}`", found.to_hex())), "{text}");
        assert!(text.contains(&DECISIONS.current_digest().to_hex()), "{text}");
        assert!(text.contains("for kind `decisions`"), "{text}");
        match error {
            PersistedSchemaError::NoUpcast { kind, found: named, current } => {
                assert_eq!(kind, "decisions");
                assert_eq!(named, found.to_hex());
                assert_eq!(current, DECISIONS.current_digest());
            }
            other @ PersistedSchemaError::Decode(_) => panic!("expected NoUpcast, got {other:?}"),
        }
    }
}
