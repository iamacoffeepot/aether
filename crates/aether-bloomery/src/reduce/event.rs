//! The reducer's input vocabulary: the closed set of admitted facts and the
//! idempotency key each arrives under (ADR-0149 §The control core).

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::gate::RecordedRefusal;
use crate::digest::Digest;
use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
use crate::values::{
    Adjudication, BaseReverify, BloomSpec, CandidateRef, CompositionParents, ConfigRegistry, Evidence,
    MemberDependency, OperatorHold, OperatorRepair, OrphanClaimRelease, OrphanClaimReleaseCompletion, ResolutionClaim,
    Statement, SuppressionDisposition, SurfaceRequest, VerifyFailureSet, Withdrawal,
};

/// An admitted fact plus its idempotency key (ADR-0149 §The control core).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Event {
    /// The idempotency key — a replayed key reduces to [`Outcome::Duplicate`](crate::Outcome::Duplicate).
    pub idempotency_key: IdempotencyKey,
    /// The fact.
    pub fact: Fact,
}

/// The closed set of admitted facts (ADR-0149 §The line: a closed enum, not a
/// workflow language).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
    /// re-checks for binding. That gate binds the signature to the question the
    /// request named ([`AuthorityDoor::Answer`](crate::AuthorityDoor),
    /// ADR-0182) *and* refuses any answer whose `parents` is not exactly that
    /// one question — this fact carries no question field of its own, so
    /// without the second refusal the submitter would still choose the reducer's
    /// target through the unsigned `parents` while the signature attested to a
    /// different one. With both, the parent scan re-checks a signed binding:
    /// `parents` is outside the signature, and a captured answer would otherwise
    /// re-point at any open hold whose question drew the same words.
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
        /// naming exactly the held question digest the host route bound its
        /// signature to.
        answer: Statement,
    },
    /// A dispatched per-member attempt completed with evidence (ADR-0149 §The
    /// line, ADR-0153). Admitted when a nonce/digest-matched attempt result
    /// arrives from evidence intake (#3502) for `Construct` or the repair-only
    /// `Refine`. The reducer evaluates the stage's
    /// completion gate against `passed` and the member's cursor: a passing gate
    /// advances the cursor and dispatches the next stage (a passing `Refine`
    /// returns to `Verify` for the delta-confirm); a failing `Construct` or
    /// `Refine` re-dispatches the same stage while the `retry_budget` allows
    /// and wedges the member once it is exhausted. The terminal `Verify` stage
    /// never routes through this fact: a pass integrates through
    /// [`Fact::Integrate`], while a failure carrying typed verifier identities
    /// routes through [`Fact::VerifyFailed`].
    ///
    /// `passed` is the completion gate's outcome as the intake broker read it from
    /// the worker's verdict — the reducer owns the *advance* decision the gate
    /// gates (advance / retry / park / wedge), never delegating that to the host; the
    /// host only reports the raw pass/fail observation. A failing Construct whose
    /// evidence kind is [`crate::EvidenceKind::ConstructDeclined`] parks rather
    /// than retrying: the lane concluded there is no candidate, and
    /// another attempt would reproduce the same refusal. Appended past
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
        /// and runs that produced nothing. Adopted onto the member's cursor on a
        /// passing completion. A failing construct's capture is recorded as the
        /// member's newest checkpoint instead of the cursor; the retry checks
        /// that commit out (#4994) but still binds the scope revision.
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
    /// The repository's mainline head, as the host observed it (#4667).
    ///
    /// `snapshot.mainline` is the base a land compare-and-swaps against, and a
    /// land is the only thing that moved it — which makes it a mirror of the
    /// repository only while blooms are mainline's sole authors. They are not:
    /// any merged pull request moves the real head, so without this fact the
    /// pointer drifts behind arbitrarily far and every bloom sealed afterwards
    /// bases on a head the repository has left.
    ///
    /// The host reads the live head and mints (or reverse-resolves) its digest;
    /// the reducer only compares. An observation that names the head mainline is
    /// already at is a no-op, so re-observing on a cadence is free. Appended past
    /// [`Fact::AggregateReviewCompleted`] so the prior facts' wire discriminants
    /// are unchanged.
    ObserveMainline {
        /// The observed head's digest, correspondence-bound to the real commit.
        head: Digest,
    },
    /// A dispatched whole-bloom aggregate verify completed with evidence — the
    /// mechanical gate over the fold, run before the critic sees it.
    ///
    /// A passing verdict dispatches the aggregate review against the same
    /// fold; a failing one re-opens every member into `Refine` (revoking each
    /// claim) until the stage's own budget is spent, then parks the bloom to
    /// the owner. Every member re-opens because a fold that does not build
    /// fails on the *combination* — the members verified individually and each
    /// passed — and over-routing is the fail-closed direction.
    ///
    /// Carries no implication for that reason, which is what distinguishes it
    /// from [`Fact::AggregateReviewCompleted`]: a critic names owners, a
    /// compiler does not. Appended past [`Fact::ObserveMainline`] so the prior
    /// facts' wire discriminants are unchanged.
    AggregateVerifyCompleted {
        /// The verified bloom.
        bloom: BloomId,
        /// The verify gate's pass/fail verdict.
        passed: bool,
        /// The verify evidence, bound to the folded tree it built — the
        /// reducer refuses a verdict whose subject is not the held fold's
        /// tree, so a stale verdict cannot act on a newer integration.
        evidence: Evidence,
    },
    /// The landing proposal's own checks failed, so it cannot merge (#4689).
    ///
    /// The last gate outside the loop. A member verifies its own candidate and
    /// [`Fact::AggregateVerifyCompleted`] verifies the fold, but neither judges
    /// the fold against a mainline that has moved since the bloom sealed — that
    /// only fails at the landing branch, downstream of every gate the bloom
    /// controls.
    ///
    /// Within the `Land` binding's retry budget this un-resolves the bloom and
    /// re-opens every member for repair; at the budget it parks to the owner.
    /// Either way the bloom stops polling a proposal nothing will accept.
    /// Appended past [`Fact::AggregateVerifyCompleted`] so the prior facts' wire
    /// discriminants are unchanged.
    LandingRejected {
        /// The bloom whose landing was refused.
        bloom: BloomId,
        /// The rejection evidence, bound to the head the proposal offered — the
        /// reducer refuses a verdict naming any other head, so a rejection from
        /// a superseded landing cannot re-open members under a newer one.
        evidence: Evidence,
    },
    /// Hand a wedged member more attempts on the bloom it already belongs to,
    /// resuming it from where it stopped (#4708).
    ///
    /// The escape from a wedge used to be supersession alone. But a bloom's
    /// identity is the digest of its spec, so re-running work that has not
    /// changed means altering something sealed — an operator fabricating a
    /// content difference to express an execution decision, and discarding the
    /// candidate the wedged member had already built along with it.
    ///
    /// A wedge is a fact about execution rather than about sealed work, which is
    /// what makes it expressible as its own fact instead of a new identity. The
    /// line against supersession follows the sealed `base`: a base that has not
    /// moved, with the scope, membership, and configuration unchanged, is a
    /// grant; anything else is a successor doing real work.
    ///
    /// Appended past [`Fact::LandingRejected`] so the prior facts' wire
    /// discriminants are unchanged.
    GrantAttempts {
        /// The bloom the wedged member belongs to.
        bloom: BloomId,
        /// The wedged member.
        workpiece: WorkpieceId,
        /// The stage the grant believes the member is wedged at — refused when
        /// it names any other, so a grant cannot act on a stale read.
        stage: StageId,
        /// How many more dispatched attempts the member may spend before it
        /// wedges again. Bounded by the stage's own
        /// [`retry_budget`](crate::StageBinding::retry_budget) in the sealed
        /// catalog, which is the whole retry authority (ADR-0177).
        attempts: u32,
    },
    /// A dispatched member Verify returned a typed failing-verifier set
    /// (ADR-0178). Appended so every prior fact retains its wire discriminant.
    VerifyFailed {
        /// The bloom whose member failed verification.
        bloom: BloomId,
        /// The member whose current cursor must be terminal Verify.
        workpiece: WorkpieceId,
        /// The failure evidence, bound to the member's current candidate tree
        /// (or its scope revision before a candidate exists).
        evidence: Evidence,
        /// The nonempty, canonical verifier identities that failed together.
        failed_verifiers: VerifyFailureSet,
    },
    /// An operator authorized releasing one orphaned claim ref (ADR-0179).
    ///
    /// A claim ref outlives the journal that created it by design — that is what
    /// makes it cross-instance — so any journal lifetime shorter than the claim's
    /// leaves a ref whose holder no surviving snapshot knows. Boot reconcile
    /// treats such a holder as foreign and report-only, and supersession needs
    /// the predecessor locally, so one orphaned mainline-admission ref refuses
    /// every later seal with nothing in-band able to act.
    ///
    /// The conservative rule stays: this fact does not loosen it, it *supplies*
    /// the proof it asks for. The reducer admits the request only while no record
    /// for `expected_holder` exists locally — a known holder belongs to the
    /// ordinary lifecycle, never to this escape hatch — and only when the
    /// authorization is an author signature asserting the exact
    /// [`ORPHAN_CLAIM_RELEASE_WORDS`](crate::ORPHAN_CLAIM_RELEASE_WORDS) over
    /// the request's own digest. Appended past [`Fact::VerifyFailed`] so the
    /// prior facts' wire discriminants are unchanged.
    RequestOrphanClaimRelease {
        /// The typed release target; its content digest is the request id.
        request: OrphanClaimRelease,
        /// The author-signed statement authorizing it. The cryptographic
        /// verification is the host route's, upstream of admission (the reducer
        /// holds no key material); the reducer re-checks the structural binding,
        /// the same trust split [`Fact::AdoptAnswer`] uses.
        authorization: Statement,
    },
    /// The release reactor finished an authorized request (ADR-0179).
    ///
    /// Terminal and journaled, so the crash window between a successful source
    /// deletion and its completion admit closes on a redrive rather than
    /// stranding the request pending forever: the redrive observes
    /// [`AlreadyAbsent`](crate::OrphanClaimReleaseCompletion::AlreadyAbsent) and
    /// completes idempotently. Appended past
    /// [`Fact::RequestOrphanClaimRelease`] so the prior facts' wire discriminants
    /// are unchanged.
    CompleteOrphanClaimRelease {
        /// The request digest this completes — refused when it names no admitted
        /// request, so a completion cannot invent one.
        request: Digest,
        /// Which terminal the source reached.
        completion: OrphanClaimReleaseCompletion,
    },
    /// A dispatched whole-bloom aggregate review reported that its executor
    /// could not judge the fold at all (ADR-0176).
    ///
    /// Distinct from a failing [`Fact::AggregateReviewCompleted`] because no
    /// candidate was judged: the reducer records the fault against the held fold
    /// and retries the same tree under a fresh order while the sealed
    /// `AggregateReview` budget allows, then records a terminal bloom-scoped
    /// wedge. It never spends
    /// [`aggregate_rolls`](crate::BloomRecord::aggregate_rolls), revokes a
    /// claim, moves a member cursor, or writes review findings — an executor
    /// outage is not something a member repair lap can fix, and charging one
    /// makes a bounded ledger lie.
    ///
    /// Appended past [`Fact::CompleteOrphanClaimRelease`] so the prior facts'
    /// wire discriminants are unchanged.
    AggregateReviewExecutorFault {
        /// The bloom whose review could not run.
        bloom: BloomId,
        /// The fault evidence, bound to the held fold's tree — the reducer
        /// refuses a fault naming any other subject, so a report from a
        /// superseded fold cannot spend a newer fold's retries.
        evidence: Evidence,
    },
    /// A cross-member fold collision (ADR-0189). The integrate reactor admits
    /// this instead of refusing in prose: `checkpoint` is the folded tree the
    /// candidate collided with, and `head` is the landable commit wrapping
    /// that tree — what the reconcile lane checks out. `evidence` carries the
    /// conflicting paths. Appended past
    /// [`Fact::AggregateReviewExecutorFault`] so the prior facts' wire
    /// discriminants are unchanged.
    FoldConflict {
        /// The bloom whose fold collided.
        bloom: BloomId,
        /// The later-folding member whose candidate conflicted.
        workpiece: WorkpieceId,
        /// The folded tree the candidate collided with.
        checkpoint: Digest,
        /// The landable head commit of that folded tree — the reconcile
        /// lane's checkout, distinct from `checkpoint` the same way
        /// [`Fact::Resolve`] carries both tree and head.
        head: Digest,
        /// The collision evidence; its `detail` names the conflicting-path
        /// report and is what a Reconcile-budget wedge attaches.
        evidence: Evidence,
    },
    /// An observation the host classified as a strict ancestor of the
    /// current mainline correspondence (#4938). The reducer names the
    /// refusal; it does not advance. A rewritten (unrelated) live ref is
    /// followable and arrives as [`Fact::ObserveMainline`] instead.
    /// Appended past [`Fact::FoldConflict`] so every prior fact keeps its
    /// wire discriminant.
    ObserveMainlineDiverged {
        /// The observed head that would move mainline backward.
        head: Digest,
    },
    /// An operator adjudicated a bloom's open composition findings (#4957) —
    /// the manager override's first move, and the only one that closes a
    /// finding without repairing it.
    ///
    /// The subject is the composition's findings channel, never a member:
    /// members are immutable after review (ADR-0191 §4), so this neither
    /// reopens nor dispatches one. Closing the findings that parked the bloom
    /// releases the park and lets the composition proceed to landing — the same
    /// place a passing review would have sent it, on the operator's authority
    /// rather than a verdict's. The reason and the operator are recorded
    /// because they are the whole audit trail of an act no gate produced.
    ///
    /// Appended past [`Fact::ObserveMainlineDiverged`] so every prior fact keeps
    /// its wire discriminant.
    OperatorAdjudication {
        /// The bloom whose findings are adjudicated.
        bloom: BloomId,
        /// What was closed, how, why, and by whom.
        adjudication: Adjudication,
    },
    /// An operator supplied a repair candidate for a wedged workpiece (#4957) —
    /// the manager override's second move: do the fix ourselves, keep the gates.
    ///
    /// The candidate re-enters at `Verify` and faces the ordinary chain, so a
    /// bad operator fix bounces exactly where a bad lane's does; only the model
    /// lap is skipped. Refused for a workpiece that has already resolved, which
    /// is the same immutability rule ADR-0191 §4 states — a repair is for work
    /// still in the line, never a second pass over finished, reviewed code.
    ///
    /// Appended past [`Fact::OperatorAdjudication`] so every prior fact keeps
    /// its wire discriminant.
    OperatorRepair {
        /// The bloom the repaired workpiece belongs to.
        bloom: BloomId,
        /// The candidate, the workpiece it is for, and who supplied it.
        repair: OperatorRepair,
    },
    /// An operator put a bloom's dispatch on the brake (#4976) — the move for a
    /// bloom that is *suspect* rather than stopped.
    ///
    /// The other operator moves all act on a bloom that has already run out of
    /// road. This one acts before that: it freezes new dispatch while the laps
    /// already running finish and journal normally, so the choice stops being
    /// "watch it spend against a refusal that will never clear, or kill the
    /// coordinator and strand what is in flight".
    ///
    /// A hold gates [`Decision::DispatchAttempt`](crate::Decision::DispatchAttempt),
    /// [`Decision::DispatchAggregateVerify`](crate::Decision::DispatchAggregateVerify),
    /// and [`Decision::DispatchAggregateReview`](crate::Decision::DispatchAggregateReview)
    /// — every new work order, including the paid critic. Claims, budgets,
    /// findings, and approval tiers are untouched, and it composes with the
    /// review park rather than replacing it: holding a parked bloom leaves the
    /// park exactly where it was.
    ///
    /// Appended past [`Fact::OperatorRepair`] so every prior fact keeps its wire
    /// discriminant.
    OperatorHold {
        /// The bloom whose dispatch is frozen.
        bloom: BloomId,
        /// Why, and by whom.
        hold: OperatorHold,
    },
    /// An operator took the brake back off (#4976).
    ///
    /// Reducing this clears the flag and re-derives what is due: every workpiece
    /// whose dispatch the hold swallowed is dispatched from the cursor it is
    /// sitting at *now*, and every aggregate gate the hold swallowed is
    /// dispatched from the fold the record is holding now. Nothing was stored
    /// at hold time to be replayed — the release re-reads the record, so the
    /// work that went out is the work the bloom is actually due, neither lost
    /// nor doubled.
    ///
    /// Appended past [`Fact::OperatorHold`] so every prior fact keeps its wire
    /// discriminant.
    OperatorRelease {
        /// The bloom being let go.
        bloom: BloomId,
        /// Why, and by whom.
        release: OperatorHold,
    },
    /// Two members of one seal declared surfaces that overlap (#4931).
    ///
    /// The door is the only place that can see this. A declared surface rides
    /// the seal *request* rather than the sealed spec — `BloomSpec` is
    /// content-addressed, so a member cannot carry one without re-digesting
    /// every bloom id — which leaves the reducer holding both memberships and
    /// neither surface. So the host, which holds every member's projection at
    /// once, intersects the pairs and states what it found; the reducer records
    /// it.
    ///
    /// A warning, never a refusal. Coarse globs over-predict — two members
    /// declaring the same crate glob routinely fold clean — so an overlap that
    /// blocked would refuse far more seals than it saved. It is journaled beside
    /// the seal so the operator deciding whether to proceed, and anyone reading
    /// back a fold conflict afterwards, sees that the door named it first.
    ///
    /// Appended past [`Fact::OperatorRelease`] so every prior fact keeps its
    /// wire discriminant.
    SurfaceOverlap {
        /// The two members whose declared surfaces intersect, in sealed
        /// membership order.
        members: Vec<WorkpieceId>,
        /// The globs both declared surfaces permit, sorted and deduplicated.
        intersection: Vec<String>,
    },
    /// A seal or supersede that carries the door-resolved member-dependency
    /// graph (ADR-0196 / ADR-0204).
    ///
    /// `Fact::Seal` / `Fact::Supersede` remain the edgeless shapes — an empty
    /// graph is those facts plus an empty
    /// [`Decision::RecordMemberDependencies`](crate::Decision::RecordMemberDependencies)
    /// appended to their effects. This variant is only the non-empty
    /// **declared** graph: a surface-derived overlap is not a dispatch gate
    /// and does not ride here, so two overlapping members with no authored
    /// edge stay `Seal`/`Supersede`. Appended past [`Fact::SurfaceOverlap`]
    /// so every prior fact keeps its wire discriminant.
    GraphSeal {
        /// The predecessor this seal supersedes, or `None` for a first seal.
        predecessor: Option<BloomId>,
        /// The spec being admitted — the same payload [`Fact::Seal`] /
        /// [`Fact::Supersede`] carry.
        spec: BloomSpec,
        /// The non-empty declared edge set the door decided (ADR-0204).
        edges: Vec<MemberDependency>,
    },
    /// A dispatched member Verify could not run because the host is missing a
    /// gate tool (#5020) — the attempt's only failure was `verify.preflight`.
    ///
    /// Distinct from [`Fact::VerifyFailed`] so the failure ledger can tell a
    /// host-provisioning gap from a candidate the gates actually judged. The
    /// reducer holds the member at `Verify`, spends no repair roll, and never
    /// dispatches Refine: there is nothing for a model to repair. Appended past
    /// [`Fact::GraphSeal`] so every prior fact keeps its wire discriminant.
    VerifyHostFault {
        /// The bloom whose member could not be verified.
        bloom: BloomId,
        /// The member sitting at terminal Verify.
        workpiece: WorkpieceId,
        /// The preflight evidence, bound to the member's current subject.
        evidence: Evidence,
        /// The preflight findings — the missing tools, listed verbatim.
        findings: String,
    },
    /// The coordinator cadence found a member held on a host fault and is
    /// re-probing its Verify (#5020).
    ///
    /// Clearing the hold and re-dispatching the same stage against the same
    /// candidate is the resume; a subsequent preflight pass runs the gates,
    /// and a subsequent preflight miss holds again. Appended past
    /// [`Fact::VerifyHostFault`] so every prior fact keeps its wire
    /// discriminant.
    ResumeHostFault {
        /// The bloom the held member belongs to.
        bloom: BloomId,
        /// The member to re-probe.
        workpiece: WorkpieceId,
    },
    /// The host assembled a dependent's multi-tip splice without a textual
    /// collision (ADR-0196 G2).
    ///
    /// `tree` is the merged artifact; `head` is the checkout commit wrapping
    /// it — the same pair [`Fact::Resolve`] carries for the weave. The
    /// reducer records `head` as the member's construct base and dispatches
    /// Construct. A residual collision arrives as [`Fact::FoldConflict`]
    /// instead. Appended past [`Fact::ResumeHostFault`] so every prior fact
    /// keeps its wire discriminant.
    SpliceAssembled {
        /// The bloom the dependent belongs to.
        bloom: BloomId,
        /// The dependent whose construct base this is.
        workpiece: WorkpieceId,
        /// The assembled tree.
        tree: Digest,
        /// The checkout commit wrapping that tree.
        head: Digest,
    },
    /// A dispatched member stage reported that its executor could not judge
    /// the subject at all (ADR-0195).
    ///
    /// Distinct from a failing [`Fact::AttemptCompleted`] or
    /// [`Fact::VerifyFailed`] because no candidate was judged: the reducer
    /// records the fault against the member's current stage and retries the
    /// same artifact under a fresh order while the sealed stage budget allows,
    /// then records a wedge whose cause is machinery. It never spends
    /// [`attempts`](crate::StageProgress::attempts),
    /// [`repair_rolls`](crate::StageProgress::repair_rolls), moves the
    /// candidate, or dispatches Refine — an executor outage is not something
    /// a member repair lap can fix.
    ///
    /// Appended past [`Fact::SpliceAssembled`] so the prior facts' wire
    /// discriminants are unchanged.
    MemberExecutorFault {
        /// The bloom whose member could not run.
        bloom: BloomId,
        /// The member sitting at the named stage.
        workpiece: WorkpieceId,
        /// The stage the faulting attempt ran — must be the member's current
        /// cursor stage, or the report is a stale/mismatched result.
        stage: StageId,
        /// The fault evidence, bound to the member's current subject — the
        /// reducer refuses a fault naming any other subject, so a report from
        /// a superseded candidate cannot spend a newer one's retries.
        evidence: Evidence,
    },
    /// The integrate fold refused at a named guard (ADR-0206).
    ///
    /// The reactor admits this instead of acking the integrate row in silence,
    /// so the served view can name the guard, the member, and the values the
    /// guard read. Appended past [`Fact::MemberExecutorFault`] so every prior
    /// fact keeps its wire discriminant.
    FoldRefused {
        /// The bloom whose fold refused.
        bloom: BloomId,
        /// The gate, guard, and values that stopped it.
        refusal: RecordedRefusal,
    },
    /// A member Verify edited a path no declared-surface glob covers (ADR-0209).
    ///
    /// Appended past [`Fact::FoldRefused`] so every prior variant keeps its wire
    /// discriminant. The reducer routes this through the same repair accounting
    /// as [`Fact::VerifyFailed`]; the paths are journal payload it never reads.
    ContainmentRefused {
        /// The bloom whose member reached outside its surface.
        bloom: BloomId,
        /// The member whose current cursor must be terminal Verify.
        workpiece: WorkpieceId,
        /// The failure evidence, bound to the member's current candidate tree
        /// (or its scope revision before a candidate exists).
        evidence: Evidence,
        /// The nonempty, canonical verifier identities that failed together,
        /// always including [`crate::VerifyFailure::Containment`].
        failed_verifiers: VerifyFailureSet,
        /// Every repository-relative path the candidate changed that no glob
        /// in the member's sealed surface covers.
        violating_paths: Vec<String>,
    },
    /// A dispatched member stage declined and named the declared-surface paths
    /// its work requires (ADR-0207).
    ///
    /// Distinct from a declining [`Fact::AttemptCompleted`] because the remedy
    /// is a person rather than a lane: the reducer parks the member awaiting a
    /// surface amendment, spends no attempt and no repair roll, and never
    /// dispatches the same stage again against the same revision. Appended past
    /// [`Fact::ContainmentRefused`] so every prior fact keeps its wire
    /// discriminant.
    SurfaceRequested {
        /// The bloom whose member declined.
        bloom: BloomId,
        /// The member awaiting the amendment.
        workpiece: WorkpieceId,
        /// The stage that declined — the member's current cursor stage, or the
        /// report is stale.
        stage: StageId,
        /// The decline evidence, bound to the member's current subject and
        /// carrying [`crate::EvidenceKind::ConstructDeclined`].
        evidence: Evidence,
        /// The normalized request the lane returned.
        request: SurfaceRequest,
    },
    /// An operator withdrew one or more members from a walking bloom (#5327).
    ///
    /// The narrow member-removal move `supersede --eject` never was: the bloom
    /// keeps its id, its sealed base, and every sibling's finished work, and
    /// only the named members leave — their lanes cancelled, their claim refs
    /// freed one at a time, their cursors dropped. A withdrawal buys no
    /// attempt, revokes no sibling's claim, and is one-way.
    ///
    /// Appended past [`Fact::SurfaceRequested`] so every prior fact keeps its
    /// wire discriminant.
    Withdraw {
        /// The walking bloom the members belong to.
        bloom: BloomId,
        /// The operator-named members, in the order the request listed them.
        /// Nonempty; each carries [`WithdrawalCause::Operator`](crate::WithdrawalCause::Operator).
        withdrawals: Vec<Withdrawal>,
        /// Also withdraw every member transitively downstream of the named
        /// ones. Without it a withdrawal that would strand a dependent is
        /// refused, naming them — a dependent left behind pins the bloom the
        /// withdrawal was meant to free.
        ///
        /// The cascade set is *derived by the reducer* rather than carried
        /// here, because journal replay folds recorded decisions rather than
        /// re-reducing facts (ADR-0190): the derived withdrawals ride the same
        /// atomic decision set as the named ones, so a replay can never land a
        /// dependent's withdrawal without its cause.
        cascade: bool,
    },
    /// The executor observed what one construct lane has written into its slot
    /// checkout (ADR-0204).
    ///
    /// Exclusivity between co-sealed members is per file and acquired at first
    /// observed write, so this is the only input the lease table has. The host
    /// reads the lane's working tree — `git status --porcelain` on the slot
    /// checkout — and reports the repository-relative paths; the reducer owns
    /// what that means for the lease table, exactly as it owns the advance
    /// decision behind a raw pass/fail observation.
    ///
    /// Restating the same set is a no-op by construction: a path the member
    /// already holds re-reads its own lease. What makes the observation
    /// idempotent against the *journal* is the admission key, which the host
    /// derives from the nonce and the observed set together, so re-observing
    /// an unchanged tree never reaches the reducer at all.
    ///
    /// A write outside the declared surface is *not* handled here. It stays a
    /// containment verify failure (ADR-0209 / #5238): the lease table answers
    /// "who else is writing this", never "may this member write at all".
    ///
    /// Appended past [`Fact::Withdraw`] so every prior fact keeps its wire
    /// discriminant.
    LaneWritesObserved {
        /// The bloom whose lanes contend.
        bloom: BloomId,
        /// The member whose lane was observed.
        workpiece: WorkpieceId,
        /// The stage the observed lane is running — the member's current
        /// cursor stage, or the observation is stale.
        stage: StageId,
        /// The repository-relative paths the lane has written, already through
        /// [`normalize_write_paths`](crate::normalize_write_paths): sorted,
        /// deduplicated, capped, and free of anything that is not one literal
        /// path inside the repository.
        paths: Vec<String>,
        /// When the host read the working tree, in unix milliseconds. The
        /// reducer holds no clock, so a lease's age — which ADR-0198 requires
        /// a lease to make visible — arrives with the observation that takes
        /// it.
        observed_at: u64,
    },
    /// A reviewer answered the suppression requests a member's candidate is
    /// carrying (ADR-0193 §5).
    ///
    /// The lane states; only a reviewer grants. A grant arrives by observation
    /// — the coordinator reads the owner-edited marker off its own landing
    /// proposal on the pass it already polls, and takes the granter from the
    /// editor login the marker check itself trusts. A denial has no marker for
    /// "no", so it arrives through a REST door of its own. Both admit here,
    /// because what is being recorded is the same thing either way: who
    /// answered what, and how.
    ///
    /// Appended past [`Fact::LaneWritesObserved`] so every prior fact keeps its
    /// wire discriminant.
    SuppressionDisposition {
        /// The bloom the answered member belongs to.
        bloom: BloomId,
        /// The member whose candidate carries the requests.
        workpiece: WorkpieceId,
        /// The answer, naming the requests it closes by digest.
        disposition: SuppressionDisposition,
    },
    /// A whole-workspace verify of a sealed base completed (ADR-0200).
    ///
    /// Appended, not inserted, so prior wire discriminants are unchanged.
    BaseVerifyCompleted {
        /// The commit the verify ran at.
        base: Digest,
        /// The tree it peeled to — the content key of the receipt.
        tree: Digest,
        /// The completion gate's pass/fail outcome.
        passed: bool,
        /// The evidence the run produced, bound to the tree it judged.
        evidence: Evidence,
        /// The verifier identities that failed together. Empty on a pass.
        failed: VerifyFailureSet,
    },
    /// The host attributed a failing fold to the two candidates that collide
    /// on it (ADR-0210).
    ///
    /// The classification is the host's because the inputs are: the failing
    /// diagnostic's paths and each candidate's diff against the sealed base
    /// live in the checkout, not in the journal. What the reducer does with it
    /// is the decision — mint the synthetic subject, and leave the member whose
    /// Verify happened to produce the verdict exactly where it was.
    ///
    /// Appended past [`Fact::BaseVerifyCompleted`] so every prior fact keeps its
    /// wire discriminant.
    CompositionNarrowed {
        /// The bloom whose fold refused.
        bloom: BloomId,
        /// The member whose Verify produced the verdict. Named so the reducer
        /// can prove it is untouched, and so an operator reading the journal
        /// can see which lane paid for discovering the collision.
        verified: WorkpieceId,
        /// The refused tree — the conflict workpiece's subject.
        tree: Digest,
        /// The commit carrying that tree, which the repair checks out.
        head: Digest,
        /// The refusing verdict.
        evidence: Evidence,
        /// The parents, the diagnostic paths, and the union bound, already
        /// through [`narrow_composition`](crate::narrow_composition).
        attribution: CompositionParents,
    },
    /// **Retired.** The machinery once granted a member the surface its lane
    /// asked for and re-pinned it mid-bloom. A widening is an operator's
    /// decision now — taken through `cargo xtask bloom amend` and delivered as
    /// a [`Fact::Supersede`] — so nothing admits this fact any more.
    ///
    /// It keeps its place because the journal already holds records written
    /// under it, and ADR-0187 never rewrites sealed history: those bytes have
    /// to go on decoding under the shape that wrote them. Kept in this
    /// declaration position so every prior fact keeps its wire discriminant.
    ///
    /// Boot replay folds each record's *recorded* decisions rather than
    /// re-deciding it (ADR-0190), so a historical grant still folds exactly as
    /// it did live and the member it moved stays where the journal put it. The
    /// reducer arm this variant reaches today refuses with
    /// `SurfaceRequestedError::GrantRetired` and decides no effects, so a fact
    /// arriving here now can neither re-pin nor re-dispatch.
    SurfaceGranted {
        /// The bloom the member belongs to.
        bloom: BloomId,
        /// The member whose surface grew.
        workpiece: WorkpieceId,
        /// The stage its lane declined at, and the stage it re-entered.
        stage: StageId,
        /// The stored successor revision, already carrying the widened surface
        /// and its approval.
        revision: Digest,
        /// The globs the grant added, at the granularity the seal admits.
        added: Vec<String>,
        /// The declining evidence the grant answered.
        evidence: Evidence,
    },
    /// An operator decided this base's red verdict does not describe the tree
    /// and asked for the gates to run again.
    ///
    /// It stamps nothing green and authorizes nothing — the gates still judge.
    /// Admitted only by the operator door. Last in declaration order, so every
    /// prior fact keeps its wire discriminant.
    BaseReverify(BaseReverify),
}

impl Fact {
    /// Every configuration registry this fact seals — the bloom-wide one and one
    /// per member, for the two facts that seal a spec, and nothing for the rest.
    ///
    /// What a caller consults to know which configuration content the reducer
    /// will need before it can decide this fact. Only the admission doors seal a
    /// registry; every other fact acts on a bloom already admitted, whose
    /// configuration was produced when it sealed.
    pub fn config_registries(&self) -> impl Iterator<Item = &ConfigRegistry> {
        let spec = match self {
            Self::Seal(spec) | Self::Supersede { successor: spec, .. } | Self::GraphSeal { spec, .. } => Some(spec),
            _ => None,
        };
        spec.into_iter().flat_map(BloomSpec::config_registries)
    }
}
