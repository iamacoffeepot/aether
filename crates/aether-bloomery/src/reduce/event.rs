//! The reducer's input vocabulary: the closed set of admitted facts and the
//! idempotency key each arrives under (ADR-0149 §The control core).

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
use crate::values::{
    BloomSpec, CandidateRef, ConfigRegistry, Evidence, OrphanClaimRelease, OrphanClaimReleaseCompletion,
    ResolutionClaim, Statement, VerifyFailureSet,
};

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
            Self::Seal(spec) | Self::Supersede { successor: spec, .. } => Some(spec),
            _ => None,
        };
        spec.into_iter().flat_map(BloomSpec::config_registries)
    }
}
