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
use crate::ids::{BloomId, IdempotencyKey, WorkpieceId};
use crate::port::{BloomView, MemberView, PendingDecisionView, ViewDocument};
use crate::values::{
    BloomSpec, Evidence, EvidenceKind, LandingReceipt, Membership, Question, ResolutionClaim, ResolvedBloom,
    StageCatalog, Statement,
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
    /// If superseded, the successor that replaced this bloom.
    pub superseded_by: Option<BloomId>,
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
        Fact::Resolve { bloom, tree, lineage } => reduce_resolve(snapshot, bloom, tree, lineage),
        Fact::Land { bloom, new_head } => reduce_land(snapshot, bloom, new_head),
    }
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
    let effects = spec
        .members()
        .iter()
        .map(|member| Decision::ClaimMembership { workpiece: member.workpiece.clone(), bloom })
        .collect();
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
/// ([`active_unlanded_bloom`]) and the boot-time claim-ref reconcile
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
    Decisions {
        outcome: Outcome::Integrated { bloom: *bloom, workpiece: claim.workpiece.clone() },
        effects: alloc::vec![Decision::RecordResolution { bloom: *bloom, claim: claim.clone() }],
    }
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

fn reduce_resolve(snapshot: &Snapshot, bloom: &BloomId, tree: &Digest, lineage: &[Digest]) -> Decisions {
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
    let resolved = ResolvedBloom { bloom: *bloom, tree: *tree, lineage: lineage.to_vec(), resolution_claims };
    Decisions {
        outcome: Outcome::Resolved(resolved.clone()),
        effects: alloc::vec![Decision::SetResolved { bloom: *bloom, resolved }],
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
            // Snapshot-inert: re-dispatch is an outbox effect the store projects,
            // rebuilt on replay from the journaled AdoptAnswer fact — it carries no
            // in-snapshot state, like EmitReceipt's outbox row.
            Decision::RedispatchStage { .. } => {}
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
