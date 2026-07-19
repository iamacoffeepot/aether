//! The control core (ADR-0149 §The control core).
//!
//! One pure function — [`reduce`] — owns every state transition. Events are
//! admitted facts with idempotency keys; decisions are value objects
//! destined for a transactional outbox; **side effects never occur inside
//! the reducer**. The journal plus the content-addressed artifact bytes are
//! the only truth, and a [`Snapshot`] is the rebuildable projection the
//! reducer reads.
//!
//! [`reduce`] *decides* — it reads a snapshot and returns [`Decisions`]. It
//! never mutates the snapshot. [`Snapshot::apply`] *evolves* — it folds a
//! decided event's effects into the next snapshot. Journal replay is
//! `reduce` then `apply`, event by event; the split keeps the decision pure
//! and the evolution mechanical.
//!
//! The active-membership uniqueness constraint (at most one active bloom per
//! workpiece) lives in the store in production (ADR-0149 §The control core);
//! the reducer enforces the same rule over its projection so seal decisions
//! are correct before the store transaction commits.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{Digest, digest_of};
use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
use crate::port::{BloomView, MemberView, PendingDecisionView, ViewDocument};
use crate::values::{
    BloomSpec, CandidateRef, Evidence, EvidenceKind, LandingReceipt, Membership, Question, ResolutionClaim,
    ResolvedBloom, StageCatalog, Statement, Transformation,
};

/// The rebuildable projection state the reducer reads (ADR-0149 §The control
/// core). Holds nothing that is not derivable from the journal.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The current mainline head — landing is a compare-and-swap against it.
    pub mainline: Digest,
    /// The active-membership map: which bloom each workpiece is claimed by.
    /// The at-most-one-active-bloom-per-workpiece constraint is this map's
    /// key uniqueness.
    pub active: BTreeMap<WorkpieceId, BloomId>,
    /// Every sealed bloom the projection knows, by id.
    pub blooms: BTreeMap<BloomId, BloomRecord>,
    /// The idempotency keys already applied — a replayed key is a no-op.
    pub seen: BTreeSet<IdempotencyKey>,
}

impl Snapshot {
    /// The genesis mainline base digest — the well-known head every fresh
    /// snapshot starts at (via [`Default`]) and every bloom seals against before
    /// any land. The boot genesis reconcile seeds this exact digest ↔ the
    /// repository's real head commit sha, so the first land's base
    /// reverse-resolves to it instead of faulting `UnresolvedCorrespondence`
    /// (issue #3615). Exposed as a named constant so the reconcile and the
    /// control core address the same genesis base.
    pub const GENESIS_MAINLINE: Digest = Digest::from_bytes([0; 32]);
}

/// The per-bloom projection record.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomRecord {
    /// The sealed, immutable spec.
    pub spec: BloomSpec,
    /// The bloom's lifecycle status.
    pub status: BloomStatus,
    /// The resolution claims accumulated by integration (and inherited from a
    /// predecessor on supersession), keyed by workpiece so a re-integration
    /// overwrites the stale candidate rather than accumulating beside it —
    /// resolve reads exactly one current claim per member (ADR-0149 §The bloom).
    pub claims: BTreeMap<WorkpieceId, ResolutionClaim>,
    /// The non-integrating evidence admitted against this bloom, in admission
    /// order (ADR-0151). Journal-derived, replay-rebuilt — a study record,
    /// verification result, or review finding is recorded here without advancing
    /// any member toward resolution. A resolution claim never lands here; it
    /// enters through `Fact::Integrate` and lives in [`claims`](Self::claims).
    pub evidence: Vec<Evidence>,
    /// The open pending-decision holds — the digests of the [`Question`]
    /// artifacts a parked attempt raised that no adopting answer has released
    /// yet (ADR-0151). Derived member state, folded from the evidence log: an
    /// admitted [`EvidenceKind::Question`] inserts its `detail` digest here, and
    /// an adopting answer removes it. A non-empty set blocks the bloom from
    /// resolving; the [`Question`]'s own `workpiece` (resolved from the digest)
    /// is what binds a hold to its member in the outward view. Journal-derived,
    /// replay-rebuilt like the rest of the record.
    pub holds: BTreeSet<Digest>,
    /// The per-member stage cursor (ADR-0149 §The line): for each member
    /// workpiece, the [`StageId`] it currently sits at plus its attempt count
    /// against that stage's `retry_budget`. Rebuilt from the journal like the rest
    /// of the record — a seal seeds every member at the entry stage
    /// ([`StageCatalog::entry_stage`]), a passing attempt advances the cursor, and
    /// a failing one bumps the attempt count in place — so replay reconstructs
    /// in-flight line position. A member drops out of the map only implicitly (it
    /// never does in V1; the record is discarded whole on supersession).
    pub progress: BTreeMap<WorkpieceId, StageProgress>,
    /// If superseded, the successor that replaced this bloom.
    pub superseded_by: Option<BloomId>,
}

/// One member's position in the per-member line: the stage it currently sits at
/// and how many attempts it has made against that stage (ADR-0149 §The line). The
/// attempt count is capped at the stage's `retry_budget`; a member at
/// `attempts == retry_budget` whose latest attempt failed is wedged — it stops
/// dispatching rather than looping.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StageProgress {
    /// The stage the member currently sits at.
    pub stage: StageId,
    /// The number of attempts dispatched against this stage so far (`1` on the
    /// first dispatch). Never exceeds the stage's `retry_budget`.
    pub attempts: u32,
    /// The candidate the member is currently at (ADR-0152) — written when a
    /// passing completion carries one, carried forward otherwise, `None` until
    /// the first capture (a member still constructing against the bare sealed
    /// base). Later dispatches re-target from it: evidence binds `tree`, the
    /// worker checks out `checkout`.
    pub candidate: Option<CandidateRef>,
    /// How many failing terminal-Verify verdicts this member has consumed
    /// (ADR-0153) — the repair ceiling's counter, carried across the Refine
    /// re-entry a failing Verify routes into (the `attempts` reset on every
    /// stage advance, so the ceiling needs its own cursor field). A failing
    /// Verify at `repair_rolls >= retry_budget_of(Verify)` wedges instead of
    /// re-entering.
    pub repair_rolls: u32,
}

/// A bloom's position in the one-way lifecycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BloomStatus {
    /// Sealed and active — the single unlanded bloom (V1 permits one per
    /// mainline).
    Sealed,
    /// Resolved: its one artifact exists, awaiting land.
    Resolved,
    /// Landed: mainline moved onto it.
    Landed,
    /// Superseded by a successor that inherited its claims.
    Superseded,
}

/// An admitted fact plus its idempotency key (ADR-0149 §The control core).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Event {
    /// The idempotency key — a replayed key reduces to [`Outcome::Duplicate`].
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
    /// [`EvidenceKind`] discriminant separates the classes; admission binds the
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
}

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
    fn rejected(outcome: Outcome) -> Self {
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
        /// The count of failing Verify verdicts consumed, this one included —
        /// the repair ceiling's cursor (wedges at Verify's retry budget).
        rolls: u32,
    },
}

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
    /// admission). A [`EvidenceKind::Question`] entry additionally folds its
    /// `detail` digest into the record's open holds (see [`BloomRecord::holds`]).
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
    /// republished to the dispatch consumer, which re-assembles the attempt's
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
        /// host driver records it without inferring it from the transformation's
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
    /// entry in the record's progress map (see [`BloomRecord::progress`]); a seal
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
    /// the transactional-outbox intent the host's land driver drains and issues
    /// through the source port's `aether.source.land` op (ADR-0149 §The boundary,
    /// migration step 3). Emitted alongside [`Decision::SetResolved`] the moment a
    /// bloom resolves: resolution is land-readiness (a resolved bloom carries its
    /// one artifact and every member's claim), so the land decision rides the same
    /// resolve commit. A snapshot-inert outbox effect like [`Decision::EmitReceipt`]
    /// / [`Decision::DispatchAttempt`] — the actual mainline advance folds in later
    /// from the driver's [`Fact::Land`] admit, never from this decision. Appended so
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
    /// integration): emitted by the [`Fact::Integrate`] that completes the claim
    /// set — every member now carries a resolution — so the host integrate
    /// driver folds each claim's candidate tree onto the bloom's integration
    /// branch in member order and admits the [`Fact::Resolve`] whose
    /// `DispatchLand` the land driver then consumes. A snapshot-inert outbox
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
    /// [`EvidenceKind::Approval`].
    UnapprovedMember(WorkpieceId),
    /// A sealed or resolved bloom already occupies the mainline. V1 permits one
    /// unlanded bloom per mainline; a successor seals via supersession instead.
    ActiveBloomExists(BloomId),
    /// The sealing spec froze a stage-catalog digest that is not the line the
    /// pipeline runs ([`StageCatalog::line_digest`]). An executed bloom is
    /// graded against the exact catalog it promised (ADR-0149 §The line), so a
    /// bloom that promises an unknown line — including the zero default — is
    /// inadmissible. V1 knows exactly one catalog; the `found` field leaves room
    /// for a known-catalog *set* later.
    UnknownStageCatalog {
        /// The unrecognized catalog digest the spec sealed.
        found: Digest,
    },
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
    /// (A resent idempotency key is caught earlier as [`Outcome::Duplicate`]; this
    /// is a *different* result naming a stage the member has already left.)
    StageMismatch {
        /// The stage the member's cursor currently sits at.
        expected: StageId,
        /// The stage the completion named.
        got: StageId,
    },
    /// The named stage is the terminal `Verify` with a passing verdict (or a
    /// passing stage otherwise off the dispatched member line): a passing
    /// `Verify` integrates the member through [`Fact::Integrate`] and never
    /// completes here, so such a completion is mis-routed.
    TerminalStage(StageId),
}

/// A land refused because mainline had moved off the bloom's sealed base.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BaseMismatch {
    /// The bloom's sealed base — the only head it may land on.
    pub expected: Digest,
    /// The base mainline was actually at.
    pub actual: Digest,
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

/// Reduce one event against a snapshot into decisions. Pure: reads the
/// snapshot, returns decisions, mutates nothing (ADR-0149 §The control core).
#[must_use]
pub fn reduce(snapshot: &Snapshot, event: &Event) -> Decisions {
    if snapshot.seen.contains(&event.idempotency_key) {
        return Decisions::rejected(Outcome::Duplicate);
    }
    match &event.fact {
        Fact::Seal(spec) => reduce_seal(snapshot, spec),
        Fact::Supersede { predecessor, successor } => reduce_supersede(snapshot, predecessor, successor),
        Fact::Integrate { bloom, claim } => reduce_integrate(snapshot, bloom, claim),
        Fact::AdmitEvidence { bloom, evidence } => reduce_admit_evidence(snapshot, bloom, evidence),
        Fact::AdoptAnswer { bloom, answer } => reduce_adopt_answer(snapshot, bloom, answer),
        Fact::AttemptCompleted { bloom, workpiece, stage, passed, evidence, candidate } => {
            reduce_attempt_completed(snapshot, bloom, workpiece, *stage, *passed, evidence, *candidate)
        }
        Fact::Resolve { bloom, tree, head, lineage } => reduce_resolve(snapshot, bloom, tree, head, lineage),
        Fact::Land { bloom, new_head } => reduce_land(snapshot, bloom, new_head),
    }
}

/// Build the seal-time entry-stage dispatch effects for one member: seed its
/// cursor at the entry stage (attempt 1) and dispatch the first attempt against
/// its frozen scope revision. The cursor advance folds into the snapshot; the
/// dispatch is a snapshot-inert outbox effect the host submits (ADR-0149 §The
/// line).
///
/// `checkout` is the git commit the attempt's worker checks out — the bloom's
/// sealed base (`spec.base()`), threaded onto the transformation so the worker
/// builds the candidate against the exact sealed source (ADR-0149 §Execution,
/// #3572). It is distinct from the member's `scope_revision` subject, which is
/// the aether content digest the returned evidence binds to.
fn entry_dispatch_effects(bloom: BloomId, member: &Membership, checkout: Digest) -> [Decision; 2] {
    let stage = StageCatalog::entry_stage();
    [
        Decision::AdvanceStage {
            bloom,
            workpiece: member.workpiece.clone(),
            progress: StageProgress { stage, attempts: 1, candidate: None, repair_rolls: 0 },
        },
        Decision::DispatchAttempt {
            bloom,
            workpiece: member.workpiece.clone(),
            stage,
            transformation: Transformation::for_member_stage(stage, member.scope_revision, checkout),
            scope_revision: member.scope_revision,
            candidate: None,
        },
    ]
}

fn reduce_seal(snapshot: &Snapshot, spec: &BloomSpec) -> Decisions {
    let bloom = spec.id();
    // A known id would resurrect and overwrite the existing record — a sealed
    // spec never amends (ADR-0149 §The bloom).
    if snapshot.blooms.contains_key(&bloom) {
        return Decisions::rejected(Outcome::SealRejected(SealError::KnownBloom(bloom)));
    }
    // Every member is verified: non-empty membership, no duplicate workpiece,
    // and each approval binds its own scope revision as an Approval (ADR-0149
    // "verify every member's scope and approval lineage").
    if let Err(error) = validate_member_admission(spec.members()) {
        return Decisions::rejected(Outcome::SealRejected(error));
    }
    // The frozen catalog must be the line the pipeline runs — a bloom is graded
    // against the exact catalog it promised (ADR-0149 §The line), so an unknown
    // catalog (including the zero default) is inadmissible.
    if let Err(error) = validate_stage_catalog(spec) {
        return Decisions::rejected(Outcome::SealRejected(error));
    }
    // All-or-nothing admission: any member already in a foreign active bloom
    // aborts the whole seal, naming the conflict — a failed batch admission
    // leaves no claims (ADR-0149 §The bloom).
    if let Some(conflict) = membership_conflict(snapshot, spec.members(), None) {
        return Decisions::rejected(Outcome::SealRejected(SealError::MembershipConflict(conflict)));
    }
    // V1 permits one sealed, unlanded bloom per mainline: refuse while any bloom
    // is still Sealed or Resolved. Landed and Superseded blooms don't block, and
    // a successor seals via Fact::Supersede — exempt, since it nets ≤1 active.
    if let Some(active) = active_unlanded_bloom(snapshot) {
        return Decisions::rejected(Outcome::SealRejected(SealError::ActiveBloomExists(active)));
    }
    // Claim each member, then seed its cursor at the entry stage and dispatch its
    // first attempt — a sealed bloom's members enter the line at `Construct`
    // (ADR-0149 §The line). The claims come first so the dispatch effects attach
    // to a member already in `active`.
    let mut effects = Vec::with_capacity(spec.members().len() * 3);
    for member in spec.members() {
        effects.push(Decision::ClaimMembership { workpiece: member.workpiece.clone(), bloom });
    }
    for member in spec.members() {
        effects.extend(entry_dispatch_effects(bloom, member, spec.base()));
    }
    Decisions { outcome: Outcome::Sealed(bloom), effects }
}

/// The per-member admission checks a bloom's membership must pass before it can
/// claim anything — a non-empty set, no duplicate workpiece, and every approval
/// binding its own scope revision as an [`EvidenceKind::Approval`] (ADR-0149
/// "verify every member's scope and approval lineage"). Both the first-door
/// seal and the second-door supersession run it, so a successor is held to the
/// same member validity a fresh seal is. The error is a [`SealError`] — seal's
/// vocabulary is the canonical name for a bad membership; supersession wraps it.
fn validate_member_admission(members: &[Membership]) -> Result<(), SealError> {
    // A bloom resolves into one artifact carrying a claim for every member; an
    // empty membership would trivially resolve and advance mainline on zero
    // evidence.
    if members.is_empty() {
        return Err(SealError::EmptyMembership);
    }
    let mut seen = BTreeSet::new();
    for member in members {
        if !seen.insert(&member.workpiece) {
            return Err(SealError::DuplicateWorkpiece(member.workpiece.clone()));
        }
        if member.approval.kind != EvidenceKind::Approval || !member.approval.validates(&member.scope_revision) {
            return Err(SealError::UnapprovedMember(member.workpiece.clone()));
        }
    }
    Ok(())
}

/// The seal-time stage-catalog admission: the sealing spec's frozen catalog
/// digest must equal [`StageCatalog::line_digest`], the one line the pipeline
/// runs (ADR-0149 §The line). Both the first-door seal and the second-door
/// supersession run it, so a successor promises the same known line a fresh
/// seal does. The error is a [`SealError`] — seal's vocabulary is canonical;
/// supersession wraps it as [`SupersedeError::InvalidMember`]. V1 has exactly
/// one known catalog, so the check is equality; the typed variant leaves room
/// for a known-catalog set later.
fn validate_stage_catalog(spec: &BloomSpec) -> Result<(), SealError> {
    let found = spec.stage_catalog();
    if found != StageCatalog::line_digest() {
        return Err(SealError::UnknownStageCatalog { found });
    }
    Ok(())
}

/// The first member already held by an active bloom other than `exempt`, if
/// any. `exempt` names a predecessor whose holds are being released in the same
/// decision set (supersession) and so are not conflicts.
fn membership_conflict(snapshot: &Snapshot, members: &[Membership], exempt: Option<&BloomId>) -> Option<SealConflict> {
    members.iter().find_map(|member| {
        let held_by = snapshot.active.get(&member.workpiece)?;
        (Some(held_by) != exempt).then(|| SealConflict { workpiece: member.workpiece.clone(), held_by: *held_by })
    })
}

/// Whether a bloom's status is active-and-unlanded — `Sealed` or `Resolved`.
/// The one predicate the V1 one-active-bloom-per-mainline seal guard
/// (`active_unlanded_bloom`) and the boot-time claim-ref reconcile
/// (`control::actor`) both read, so the "which blooms hold claim refs" question
/// has a single answer both sides cannot drift from (ADR-0150 §The claim
/// registry).
#[must_use]
pub fn is_active_unlanded(status: BloomStatus) -> bool {
    matches!(status, BloomStatus::Sealed | BloomStatus::Resolved)
}

/// The id of a bloom still `Sealed` or `Resolved` (an unlanded active bloom),
/// if any — the input to the V1 one-active-bloom-per-mainline guard.
fn active_unlanded_bloom(snapshot: &Snapshot) -> Option<BloomId> {
    snapshot.blooms.iter().find(|(_, record)| is_active_unlanded(record.status)).map(|(id, _)| *id)
}

fn reduce_supersede(snapshot: &Snapshot, predecessor: &BloomId, successor: &BloomSpec) -> Decisions {
    let Some(record) = snapshot.blooms.get(predecessor) else {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::UnknownOrInactivePredecessor));
    };
    // The ADR's primary supersession trigger is a failed land, which happens at
    // Resolved — so both Sealed and Resolved predecessors are supersedable, or
    // a failed land wedges the bloom permanently (ADR-0149 §The bloom).
    if !matches!(record.status, BloomStatus::Sealed | BloomStatus::Resolved) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::UnknownOrInactivePredecessor));
    }
    let successor_id = successor.id();
    if successor_id == *predecessor {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::SelfSupersession));
    }
    // The successor is a second admission into `active`; a successor id that
    // collides with some other already-known bloom would resurrect and
    // overwrite that bloom's record, mirroring `reduce_seal`'s `KnownBloom`
    // guard on the seal door.
    if snapshot.blooms.contains_key(&successor_id) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::KnownSuccessor(successor_id)));
    }
    // The successor is a fresh membership set claiming into `active`, so it must
    // pass the same per-member admission a seal runs before it claims or inherits
    // anything — an empty, duplicate-workpiece, or unapproved successor is
    // refused here rather than admitted on invalid members (ADR-0149).
    if let Err(error) = validate_member_admission(successor.members()) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::InvalidMember(error)));
    }
    // A superseding spec is held to seal's catalog admission too — it must
    // promise the same known line (ADR-0149 §The line).
    if let Err(error) = validate_stage_catalog(successor) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::InvalidMember(error)));
    }
    // Supersession is a second door into `active`, so it runs the same
    // all-or-nothing conflict scan as seal — but the predecessor's own holds are
    // released in this decision set, so only a foreign bloom's hold conflicts.
    if let Some(conflict) = membership_conflict(snapshot, successor.members(), Some(predecessor)) {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::MembershipConflict(conflict)));
    }
    let mut effects = Vec::new();
    // Release the predecessor's memberships, then claim the successor's, then
    // inherit the predecessor's still-valid resolution claims, then name it
    // superseded — one decision set, applied atomically (a successor atomically
    // inherits its predecessor's claims, ADR-0149 §The bloom).
    for member in record.spec.members() {
        effects.push(Decision::ReleaseMembership { workpiece: member.workpiece.clone(), bloom: *predecessor });
    }
    for member in successor.members() {
        effects.push(Decision::ClaimMembership { workpiece: member.workpiece.clone(), bloom: successor_id });
    }
    // Inherit only a claim whose workpiece the successor re-admits at the same
    // scope revision — an ejected or scope-changed workpiece drops its stale
    // claim (ADR-0149 §The bloom).
    for claim in record.claims.values() {
        let still_valid = successor
            .members()
            .iter()
            .any(|m| m.workpiece == claim.workpiece && m.scope_revision == claim.scope_revision);
        if still_valid {
            effects.push(Decision::InheritClaim { bloom: successor_id, claim: claim.clone() });
        }
    }
    effects.push(Decision::MarkSuperseded { bloom: *predecessor, by: successor_id });
    Decisions { outcome: Outcome::Superseded { predecessor: *predecessor, successor: successor_id }, effects }
}

fn reduce_integrate(snapshot: &Snapshot, bloom: &BloomId, claim: &ResolutionClaim) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::IntegrateRejected(IntegrateError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::IntegrateRejected(IntegrateError::UnknownOrInactiveBloom));
    }
    if !record.spec.members().iter().any(|m| m.workpiece == claim.workpiece) {
        return Decisions::rejected(Outcome::IntegrateRejected(IntegrateError::NotAMember));
    }
    // Evidence must bind to the exact candidate it claims — no evidence
    // validates a digest it does not name (ADR-0149 §The value vocabulary).
    if !claim.evidence.validates(&claim.candidate) {
        return Decisions::rejected(Outcome::IntegrateRejected(IntegrateError::EvidenceNotBound));
    }
    let mut effects = alloc::vec![Decision::RecordResolution { bloom: *bloom, claim: claim.clone() }];
    // The claim that completes the set dispatches integration (ADR-0152
    // §Resolution drives integration): with every member now carrying a
    // resolution, the host driver folds each claimed candidate tree onto the
    // integration branch in member order and admits the resulting
    // `Fact::Resolve`. The snapshot has not folded this claim yet, so the
    // completeness check counts it alongside the recorded ones.
    let complete = record
        .spec
        .members()
        .iter()
        .all(|member| member.workpiece == claim.workpiece || record.claims.contains_key(&member.workpiece));
    if complete {
        let candidates = record
            .spec
            .members()
            .iter()
            .filter_map(|member| {
                if member.workpiece == claim.workpiece {
                    Some(claim.candidate)
                } else {
                    record.claims.get(&member.workpiece).map(|recorded| recorded.candidate)
                }
            })
            .collect();
        effects.push(Decision::DispatchIntegration { bloom: *bloom, base: record.spec.base(), candidates });
    }
    Decisions { outcome: Outcome::Integrated { bloom: *bloom, workpiece: claim.workpiece.clone() }, effects }
}

/// Admit non-integrating evidence into a bloom's evidence log (ADR-0151). Runs
/// the same active-bloom guard `reduce_integrate` does — evidence records only
/// against a `Sealed` bloom, before it resolves — and the same
/// bind-to-its-own-class refusal: a resolving [`ResolutionClaim`] enters through
/// [`Fact::Integrate`] and an [`EvidenceKind::Approval`] seals a member, so
/// neither is bound to the free evidence-log door. The four non-integrating
/// classes (`VerificationResult`, `ReviewFinding`, `StudyRecord`, `Question`)
/// are what this log records; a mis-routed integrating/approval class is
/// [`AdmitEvidenceError::EvidenceNotBound`]. A `Question` entry additionally
/// derives a pending-decision hold in the fold (see [`BloomRecord::holds`]).
/// The evidence carries its own
/// subject digest, so no separate candidate binding is threaded through the
/// fact (unlike integrate, which binds a claim's evidence to its candidate).
fn reduce_admit_evidence(snapshot: &Snapshot, bloom: &BloomId, evidence: &Evidence) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::AdmitEvidenceRejected(AdmitEvidenceError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::AdmitEvidenceRejected(AdmitEvidenceError::UnknownOrInactiveBloom));
    }
    // Only the non-integrating classes bind to the evidence log — a resolution
    // claim integrates and an approval seals, each through its own door
    // (ADR-0151: a `ResolutionClaim` never enters through `AdmitEvidence`).
    if !matches!(
        evidence.kind,
        EvidenceKind::VerificationResult
            | EvidenceKind::ReviewFinding
            | EvidenceKind::StudyRecord
            | EvidenceKind::Question
    ) {
        return Decisions::rejected(Outcome::AdmitEvidenceRejected(AdmitEvidenceError::EvidenceNotBound));
    }
    Decisions {
        outcome: Outcome::EvidenceAdmitted { bloom: *bloom, subject: evidence.subject },
        effects: alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }],
    }
}

/// Adopt an answer to a parked question (ADR-0151, [`Fact::AdoptAnswer`]).
///
/// The reducer's structural gate: the bloom is active, the answer is
/// instruction-capable (an author signature — only that provenance becomes
/// intent), and its `parents` name a digest that is an open hold on the bloom.
/// On admit it releases that hold and re-dispatches the held stage with the
/// answer digest in the attempt's input closure. The cryptographic
/// `verify_authority` check is the host answer route's, upstream of admission
/// (the reducer holds no key material) — the same trust split the intake broker
/// uses for evidence, where the reducer re-checks binding but not the signature.
///
/// An answer whose parents name several open holds releases the first one in
/// digest order; a parked question raises one hold per member, so the common
/// case names exactly one.
fn reduce_adopt_answer(snapshot: &Snapshot, bloom: &BloomId, answer: &Statement) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::AdoptAnswerRejected(AdoptAnswerError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::AdoptAnswerRejected(AdoptAnswerError::UnknownOrInactiveBloom));
    }
    // Only an author signature can become intent — a non-author statement can
    // never adopt a question (ADR-0149 §The value vocabulary).
    if !answer.is_instruction_capable() {
        return Decisions::rejected(Outcome::AdoptAnswerRejected(AdoptAnswerError::NotInstructionCapable));
    }
    // The answer adopts a question by naming its exact digest in its parents;
    // the first parent that is an open hold is the released question (holds is a
    // BTreeSet, so the scan is deterministic).
    let Some(question) = answer.parents.iter().find(|parent| record.holds.contains(parent)).copied() else {
        return Decisions::rejected(Outcome::AdoptAnswerRejected(AdoptAnswerError::NoMatchingHold));
    };
    let answer_digest = digest_of(answer);
    Decisions {
        outcome: Outcome::AnswerAdopted { bloom: *bloom, question },
        effects: alloc::vec![
            Decision::ReleaseHold { bloom: *bloom, question },
            Decision::RedispatchStage { bloom: *bloom, question, answer: answer_digest },
        ],
    }
}

/// Reduce a per-member attempt completion (ADR-0149 §The line, [`Fact::AttemptCompleted`]).
///
/// The reducer alone advances line position: it reads the member's cursor,
/// evaluates the stage's completion gate against the host-reported `passed`
/// signal, and decides advance / retry / wedge — the host submits transformations
/// and reports raw outcomes but never advances state (the ADR-0149 invariant, and
/// the reason the "host evaluates the gate" alternative was rejected).
///
/// A passing gate advances the cursor to the next member stage and dispatches it
/// (a passing repair-only `Refine` returns to `Verify` for the delta-confirm,
/// ADR-0153); a failing gate re-dispatches the same stage while the stage's
/// `retry_budget` allows and wedges the member once it is exhausted (a wedged
/// member stops dispatching — a supersession is the escape). The attempt's
/// evidence is recorded in the bloom's evidence log either way. The terminal
/// `Verify` is the exception to same-stage retry: a *passing* `Verify` integrates
/// the member through [`Fact::Integrate`] and never completes here (a passing
/// terminal completion is a mis-route,
/// [`AttemptCompletedError::TerminalStage`]); a *failing* `Verify` re-enters
/// `Refine` — the findings-directed fix, since re-running the mechanical gate on
/// an unchanged candidate changes nothing — bounded by Verify's `retry_budget`
/// over the cursor-carried `repair_rolls`, wedging once the budget's worth of
/// failing verdicts is consumed.
fn reduce_attempt_completed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    passed: bool,
    evidence: &Evidence,
    captured: Option<CandidateRef>,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::UnknownOrInactiveBloom));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::NotAMember(
            workpiece.clone(),
        )));
    };
    // A *passing* terminal `Verify` is a mis-route — a passing Verify integrates
    // through `Fact::Integrate` and never completes here, so a passing completion
    // whose stage has no successor is rejected. A *failing* `Verify` does complete
    // here (the Refine re-entry below), so the guard fires only on the passing
    // terminal case; a mis-routed passing terminal is caught before the cursor
    // check so it reads as `TerminalStage` rather than a `StageMismatch`. The
    // repair-only `Refine` sits off the standing line (ADR-0153) with an explicit
    // successor: its pass returns the member to `Verify` for the delta-confirm.
    let next = if stage == StageId::Refine {
        Some(StageId::Verify)
    } else {
        StageCatalog::next_member_stage(stage)
    };
    if passed && next.is_none() {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::TerminalStage(stage)));
    }
    // The completion must name the member's current cursor stage; a result for a
    // stage the member has already left is stale/out-of-order and is not acted on.
    let cursor = record.progress.get(workpiece).copied();
    if cursor.map(|progress| progress.stage) != Some(stage) {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::StageMismatch {
            expected: cursor.map_or_else(StageCatalog::entry_stage, |progress| progress.stage),
            got: stage,
        }));
    }
    let attempts = cursor.map_or(1, |progress| progress.attempts);
    // The member's candidate after this completion (ADR-0152): a passing attempt
    // adopts the capture it carried (a mechanical lane carries none — the prior
    // candidate rides forward); a failing attempt adopts nothing, so its capture
    // is discarded and the member stays at the candidate its last pass produced.
    let prior = cursor.and_then(|progress| progress.candidate);
    let candidate = if passed {
        captured.or(prior)
    } else {
        prior
    };
    // The dispatch targets re-resolve from the cursor (ADR-0152): with a
    // candidate present, the returned evidence binds its tree and the worker
    // checks out its capture commit; without one, the member's frozen scope
    // revision and the bloom's sealed base (ADR-0149 §Execution, #3572).
    let (subject, checkout) = candidate
        .map_or_else(|| (member.scope_revision, record.spec.base()), |current| (current.tree, current.checkout));
    // The attempt result is journaled evidence about the member, recorded whatever
    // the gate decides.
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    // A passing gate advances the cursor to the next member stage and dispatches
    // it. `next` is `Some` on this branch — a passing terminal completion was
    // rejected above, so a passing stage always has a successor.
    let repair_rolls = cursor.map_or(0, |progress| progress.repair_rolls);
    if let Some(next) = next.filter(|_| passed) {
        effects.push(Decision::AdvanceStage {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            progress: StageProgress { stage: next, attempts: 1, candidate, repair_rolls },
        });
        effects.push(Decision::DispatchAttempt {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            stage: next,
            transformation: Transformation::for_member_stage(next, subject, checkout),
            scope_revision: member.scope_revision,
            candidate: candidate.map(|current| current.tree),
        });
        return Decisions {
            outcome: Outcome::AttemptAdvanced { bloom: *bloom, workpiece: workpiece.clone(), from: stage, to: next },
            effects,
        };
    }
    // A failing terminal Verify re-enters Refine instead of re-running the
    // mechanical gate on an unchanged candidate (ADR-0153): only a
    // findings-directed fix changes the next verdict, so the member routes back
    // to the repair stage that can produce one (the host threads the persisted
    // failure findings onto the dispatch, #3656). The ceiling is Verify's retry
    // budget over `repair_rolls` — the cursor-carried count the per-stage
    // `attempts` reset cannot clear — so once the budget's worth of failing
    // verdicts is consumed the member wedges: never an extra roll, never a
    // silent integrate.
    if stage == StageId::Verify {
        let rolls = repair_rolls + 1;
        if rolls < StageCatalog::retry_budget_of(StageId::Verify).unwrap_or(1) {
            effects.push(Decision::AdvanceStage {
                bloom: *bloom,
                workpiece: workpiece.clone(),
                progress: StageProgress { stage: StageId::Refine, attempts: 1, candidate, repair_rolls: rolls },
            });
            effects.push(Decision::DispatchAttempt {
                bloom: *bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Refine,
                transformation: Transformation::for_member_stage(StageId::Refine, subject, checkout),
                scope_revision: member.scope_revision,
                candidate: candidate.map(|current| current.tree),
            });
            return Decisions {
                outcome: Outcome::RefineReentered { bloom: *bloom, workpiece: workpiece.clone(), rolls },
                effects,
            };
        }
        return Decisions {
            outcome: Outcome::AttemptWedged { bloom: *bloom, workpiece: workpiece.clone(), stage },
            effects,
        };
    }
    // A failing gate re-dispatches the same stage while its retry budget allows;
    // an exhausted budget wedges the member — it stops dispatching rather than
    // looping (the tripwire).
    let budget = StageCatalog::retry_budget_of(stage).unwrap_or(1);
    if attempts < budget {
        let attempt = attempts + 1;
        effects.push(Decision::AdvanceStage {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            progress: StageProgress { stage, attempts: attempt, candidate, repair_rolls },
        });
        effects.push(Decision::DispatchAttempt {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            stage,
            transformation: Transformation::for_member_stage(stage, subject, checkout),
            scope_revision: member.scope_revision,
            candidate: candidate.map(|current| current.tree),
        });
        return Decisions {
            outcome: Outcome::AttemptRetried { bloom: *bloom, workpiece: workpiece.clone(), stage, attempt },
            effects,
        };
    }
    Decisions { outcome: Outcome::AttemptWedged { bloom: *bloom, workpiece: workpiece.clone(), stage }, effects }
}

fn reduce_resolve(snapshot: &Snapshot, bloom: &BloomId, tree: &Digest, head: &Digest, lineage: &[Digest]) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::ResolveRejected(ResolveError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::ResolveRejected(ResolveError::UnknownOrInactiveBloom));
    }
    // A member held on a parked question cannot integrate, so a bloom with any
    // open hold cannot resolve (ADR-0151) — refused before the per-member claim
    // scan so the pending decision is the named reason, not a bare
    // MemberNotIntegrated.
    if let Some(question) = record.holds.iter().next().copied() {
        return Decisions::rejected(Outcome::ResolveRejected(ResolveError::PendingDecision { question }));
    }
    // Every frozen member must carry a resolution claim before the bloom can
    // resolve — a resolved bloom carries a claim for every member (ADR-0149
    // §The bloom).
    let mut resolution_claims = Vec::with_capacity(record.spec.members().len());
    for member in record.spec.members() {
        let Some(claim) = record.claims.get(&member.workpiece) else {
            return Decisions::rejected(Outcome::ResolveRejected(ResolveError::MemberNotIntegrated {
                workpiece: member.workpiece.clone(),
            }));
        };
        resolution_claims.push(claim.clone());
    }
    let resolved =
        ResolvedBloom { bloom: *bloom, tree: *tree, head: *head, lineage: lineage.to_vec(), resolution_claims };
    // Resolution is land-readiness: the bloom now carries its one artifact and a
    // claim for every member, so the source-port CAS land can be driven. Emit the
    // land decision on the same resolve commit — the host land driver drains it,
    // issues the CAS against `expected_base`, and admits `Fact::Land` on success
    // (ADR-0149 migration step 3). `new_head` is the resolved integrated head
    // commit's digest (distinct from the artifact `tree`), the head mainline
    // advances to; the reducer never does the I/O.
    Decisions {
        outcome: Outcome::Resolved(resolved.clone()),
        effects: alloc::vec![
            Decision::SetResolved { bloom: *bloom, resolved },
            Decision::DispatchLand { bloom: *bloom, expected_base: record.spec.base(), new_head: *head },
        ],
    }
}

fn reduce_land(snapshot: &Snapshot, bloom: &BloomId, new_head: &Digest) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::LandRejected(LandError::UnknownBloom(*bloom)));
    };
    if record.status != BloomStatus::Resolved {
        return Decisions::rejected(Outcome::LandRejected(LandError::NotResolved(*bloom)));
    }
    // Compare-and-swap against the bloom's own sealed base — the only head a V1
    // bloom may land on. A moved mainline forces supersession, never a land onto
    // the new head (ADR-0149 §The bloom).
    let base = record.spec.base();
    if snapshot.mainline != base {
        return Decisions::rejected(Outcome::LandRejected(LandError::BaseMismatch(BaseMismatch {
            expected: base,
            actual: snapshot.mainline,
        })));
    }
    let receipt = LandingReceipt { bloom: *bloom, previous_base: base, new_head: *new_head };
    // Release the landed bloom's memberships from `active`, then advance
    // mainline and emit the receipt — one atomic decision set (m5: a land frees
    // its workpieces so the next bloom may seal them).
    let mut effects: Vec<Decision> = record
        .spec
        .members()
        .iter()
        .map(|member| Decision::ReleaseMembership { workpiece: member.workpiece.clone(), bloom: *bloom })
        .collect();
    effects.push(Decision::AdvanceMainline { from: snapshot.mainline, to: *new_head });
    effects.push(Decision::EmitReceipt(receipt.clone()));
    Decisions { outcome: Outcome::Landed(receipt), effects }
}

impl Snapshot {
    /// A fresh snapshot at a mainline base, with no blooms.
    #[must_use]
    pub fn new(mainline: Digest) -> Self {
        Self { mainline, ..Self::default() }
    }

    /// Evolve the snapshot by applying a decided event's effects — the
    /// mechanical counterpart to [`reduce`]'s decision. Registers a newly
    /// sealed or superseding bloom from the fact, folds every [`Decision`],
    /// and records the idempotency key. A duplicate or rejected outcome
    /// still records the key (so a replay stays a no-op) but changes nothing
    /// else.
    #[must_use]
    pub fn apply(&self, event: &Event, decisions: &Decisions) -> Self {
        let mut next = self.clone();
        next.seen.insert(event.idempotency_key.clone());
        // Register the bloom the fact seals, before its membership claims
        // land, so the claim/inherit effects have a record to attach to.
        match (&event.fact, &decisions.outcome) {
            (Fact::Seal(spec), Outcome::Sealed(id)) => {
                next.blooms.insert(*id, BloomRecord::sealed(spec.clone()));
            }
            (Fact::Supersede { successor, .. }, Outcome::Superseded { successor: id, .. }) => {
                next.blooms.insert(*id, BloomRecord::sealed(successor.clone()));
            }
            _ => {}
        }
        for effect in &decisions.effects {
            next.apply_effect(effect);
        }
        next
    }

    fn apply_effect(&mut self, effect: &Decision) {
        match effect {
            Decision::ClaimMembership { workpiece, bloom } => {
                self.active.insert(workpiece.clone(), *bloom);
            }
            Decision::ReleaseMembership { workpiece, bloom } => {
                if self.active.get(workpiece) == Some(bloom) {
                    self.active.remove(workpiece);
                }
            }
            Decision::InheritClaim { bloom, claim } | Decision::RecordResolution { bloom, claim } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    // Keyed by workpiece: a re-integration overwrites the stale
                    // candidate rather than accumulating a second claim.
                    record.claims.insert(claim.workpiece.clone(), claim.clone());
                }
            }
            Decision::RecordEvidence { bloom, evidence } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    // Append in admission order — the evidence log is a growing
                    // journal-derived history, not a keyed latest-wins map.
                    record.evidence.push(evidence.clone());
                    // A question admission derives a pending-decision hold as part
                    // of this same fold (ADR-0151, no new fact): the question digest
                    // (the evidence detail) blocks the bloom until an answer releases
                    // it.
                    if evidence.kind == EvidenceKind::Question {
                        record.holds.insert(evidence.detail);
                    }
                }
            }
            Decision::ReleaseHold { bloom, question } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.holds.remove(question);
                }
            }
            Decision::AdvanceStage { bloom, workpiece, progress } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.progress.insert(workpiece.clone(), *progress);
                }
            }
            // Snapshot-inert outbox effects the store projects and republishes,
            // rebuilt on replay from the journaled fact — they carry no in-snapshot
            // state, like EmitReceipt's outbox row. A dispatch's paired cursor rides
            // its sibling AdvanceStage; a re-dispatch's hold release rides ReleaseHold.
            Decision::RedispatchStage { .. }
            | Decision::DispatchAttempt { .. }
            | Decision::DispatchLand { .. }
            | Decision::DispatchIntegration { .. } => {}
            Decision::MarkSuperseded { bloom, by } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.status = BloomStatus::Superseded;
                    record.superseded_by = Some(*by);
                }
            }
            Decision::SetResolved { bloom, .. } => {
                if let Some(record) = self.blooms.get_mut(bloom) {
                    record.status = BloomStatus::Resolved;
                }
            }
            Decision::AdvanceMainline { to, .. } => {
                self.mainline = *to;
            }
            Decision::EmitReceipt(receipt) => {
                if let Some(record) = self.blooms.get_mut(&receipt.bloom) {
                    record.status = BloomStatus::Landed;
                }
            }
        }
    }
}

impl BloomRecord {
    fn sealed(spec: BloomSpec) -> Self {
        Self {
            spec,
            status: BloomStatus::Sealed,
            claims: BTreeMap::new(),
            evidence: Vec::new(),
            holds: BTreeSet::new(),
            progress: BTreeMap::new(),
            superseded_by: None,
        }
    }
}

/// Assemble a self-contained [`ViewDocument`] from a snapshot — the pure
/// `Snapshot -> ViewDocument` projection the reconcile port pushes outward
/// (ADR-0149 §The boundary, as amended by [#3471]). Every field an adapter
/// renders rides on the returned document, so the adapter never queries back
/// into the store. Pure: reads the snapshot, allocates a document, mutates
/// nothing.
///
/// Each [`BloomRecord`] becomes a [`BloomView`] (its sealed-spec id, status,
/// and successor), and each sealed [`crate::Membership`] a [`MemberView`]
/// carrying the member's scope revision, approval evidence, — matched by
/// workpiece from the record's accumulated claims — its resolution claim once
/// integrated (`None` until then), and — matched by workpiece from the
/// [`Question`] each open hold resolves to — its pending-decision hold (`None`
/// when the member is not held).
///
/// `resolve_question` resolves an open hold's question digest to its
/// [`Question`] bytes, the same injected read-only resolver
/// [`grade`](crate::study_report::grade) uses for study records: the reducer's
/// snapshot holds question *digests*, not the rendered prompt/options or the
/// member the hold binds to, so a snapshot-only signature could carry neither.
/// A hold whose bytes the resolver cannot read (a caller with no artifact
/// access, e.g. the live-query path) surfaces no `pending_decision` on its
/// member, exactly as an unresolvable study record contributes no cost to a
/// grade.
///
/// [#3471]: https://github.com/iamacoffeepot/aether/issues/3471
#[must_use]
pub fn view_of(snapshot: &Snapshot, resolve_question: impl Fn(&Digest) -> Option<Question>) -> ViewDocument {
    let blooms = snapshot
        .blooms
        .values()
        .map(|record| {
            // Resolve each open hold once, then bind it to the member it names —
            // a parked question raises one hold per member, so the map is small.
            let held: Vec<(WorkpieceId, PendingDecisionView)> = record
                .holds
                .iter()
                .filter_map(|digest| {
                    let question = resolve_question(digest)?;
                    Some((
                        question.workpiece.clone(),
                        PendingDecisionView {
                            question: *digest,
                            stage: question.stage,
                            prompt: question.prompt,
                            options: question.options,
                            blocked: question.blocked,
                        },
                    ))
                })
                .collect();
            let members = record
                .spec
                .members()
                .iter()
                .map(|member| MemberView {
                    workpiece: member.workpiece.clone(),
                    scope_revision: member.scope_revision,
                    approval: member.approval.clone(),
                    resolution: record.claims.get(&member.workpiece).cloned(),
                    pending_decision: held
                        .iter()
                        .find(|(workpiece, _)| *workpiece == member.workpiece)
                        .map(|(_, view)| view.clone()),
                })
                .collect();
            BloomView { id: record.spec.id(), status: record.status, superseded_by: record.superseded_by, members }
        })
        .collect();
    ViewDocument { mainline: snapshot.mainline, blooms }
}
