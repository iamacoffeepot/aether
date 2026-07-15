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

use crate::digest::Digest;
use crate::ids::{BloomId, IdempotencyKey, WorkpieceId};
use crate::port::{BloomView, MemberView, ViewDocument};
use crate::values::{BloomSpec, LandingReceipt, ResolutionClaim, ResolvedBloom};

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
    /// predecessor on supersession).
    pub claims: Vec<ResolutionClaim>,
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
    /// Resolve a bloom into its one artifact, once every member is integrated.
    Resolve {
        /// The bloom being resolved.
        bloom: BloomId,
        /// The final integrated tree digest.
        tree: Digest,
        /// The integration lineage.
        lineage: Vec<Digest>,
    },
    /// Land a resolved bloom by compare-and-swap against the expected base.
    Land {
        /// The bloom being landed.
        bloom: BloomId,
        /// The base the caller expects mainline to still be at.
        expected_base: Digest,
        /// The new mainline head.
        new_head: Digest,
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
    /// A seal was refused, naming the conflicting workpiece.
    SealRejected(SealConflict),
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
    /// A bloom resolved into its one artifact.
    Resolved(ResolvedBloom),
    /// A resolve was refused.
    ResolveRejected(ResolveError),
    /// A bloom landed.
    Landed(LandingReceipt),
    /// A land was refused: mainline had moved off the expected base.
    LandRejected(BaseMismatch),
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
}

/// A seal refused because a workpiece is already in an active bloom.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SealConflict {
    /// The workpiece already claimed.
    pub workpiece: WorkpieceId,
    /// The active bloom holding it.
    pub held_by: BloomId,
}

/// Why a supersession was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SupersedeError {
    /// The predecessor is not a known, active bloom.
    UnknownOrInactivePredecessor,
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
}

/// A land refused because mainline had moved off the expected base.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BaseMismatch {
    /// The base the caller expected.
    pub expected: Digest,
    /// The base mainline was actually at.
    pub actual: Digest,
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
        Fact::Resolve { bloom, tree, lineage } => reduce_resolve(snapshot, bloom, tree, lineage),
        Fact::Land { bloom, expected_base, new_head } => reduce_land(snapshot, bloom, expected_base, new_head),
    }
}

fn reduce_seal(snapshot: &Snapshot, spec: &BloomSpec) -> Decisions {
    let bloom = spec.id();
    // All-or-nothing admission: the first member already in an active bloom
    // aborts the whole seal, naming the conflict — a failed batch admission
    // leaves no claims (ADR-0149 §The bloom).
    for member in spec.members() {
        if let Some(held_by) = snapshot.active.get(&member.workpiece) {
            return Decisions::rejected(Outcome::SealRejected(SealConflict {
                workpiece: member.workpiece.clone(),
                held_by: *held_by,
            }));
        }
    }
    let effects = spec
        .members()
        .iter()
        .map(|member| Decision::ClaimMembership { workpiece: member.workpiece.clone(), bloom })
        .collect();
    Decisions { outcome: Outcome::Sealed(bloom), effects }
}

fn reduce_supersede(snapshot: &Snapshot, predecessor: &BloomId, successor: &BloomSpec) -> Decisions {
    let Some(record) = snapshot.blooms.get(predecessor) else {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::UnknownOrInactivePredecessor));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::SupersedeRejected(SupersedeError::UnknownOrInactivePredecessor));
    }
    let successor_id = successor.id();
    let mut effects = Vec::new();
    // Release the predecessor's memberships, then claim the successor's, then
    // inherit the predecessor's resolution claims, then name it superseded —
    // one decision set, applied atomically (a successor atomically inherits
    // its predecessor's claims, ADR-0149 §The bloom).
    for member in record.spec.members() {
        effects.push(Decision::ReleaseMembership { workpiece: member.workpiece.clone(), bloom: *predecessor });
    }
    for member in successor.members() {
        effects.push(Decision::ClaimMembership { workpiece: member.workpiece.clone(), bloom: successor_id });
    }
    for claim in &record.claims {
        effects.push(Decision::InheritClaim { bloom: successor_id, claim: claim.clone() });
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

fn reduce_resolve(snapshot: &Snapshot, bloom: &BloomId, tree: &Digest, lineage: &[Digest]) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::ResolveRejected(ResolveError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::ResolveRejected(ResolveError::UnknownOrInactiveBloom));
    }
    // Every frozen member must carry a resolution claim before the bloom can
    // resolve — a resolved bloom carries a claim for every member (ADR-0149
    // §The bloom).
    let mut resolution_claims = Vec::with_capacity(record.spec.members().len());
    for member in record.spec.members() {
        let Some(claim) = record.claims.iter().find(|c| c.workpiece == member.workpiece) else {
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

fn reduce_land(snapshot: &Snapshot, bloom: &BloomId, expected_base: &Digest, new_head: &Digest) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::LandRejected(BaseMismatch {
            expected: *expected_base,
            actual: snapshot.mainline,
        }));
    };
    if record.status != BloomStatus::Resolved {
        return Decisions::rejected(Outcome::LandRejected(BaseMismatch {
            expected: *expected_base,
            actual: snapshot.mainline,
        }));
    }
    // Compare-and-swap: land only if mainline is still the sealed base
    // (ADR-0149 §The bloom).
    if snapshot.mainline != *expected_base {
        return Decisions::rejected(Outcome::LandRejected(BaseMismatch {
            expected: *expected_base,
            actual: snapshot.mainline,
        }));
    }
    let receipt = LandingReceipt { bloom: *bloom, previous_base: *expected_base, new_head: *new_head };
    Decisions {
        outcome: Outcome::Landed(receipt.clone()),
        effects: alloc::vec![
            Decision::AdvanceMainline { from: snapshot.mainline, to: *new_head },
            Decision::EmitReceipt(receipt),
        ],
    }
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
                    record.claims.push(claim.clone());
                }
            }
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
        Self { spec, status: BloomStatus::Sealed, claims: Vec::new(), superseded_by: None }
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
/// carrying the member's scope revision, approval evidence, and — matched by
/// workpiece from the record's accumulated claims — its resolution claim once
/// integrated (`None` until then).
///
/// [#3471]: https://github.com/iamacoffeepot/aether/issues/3471
#[must_use]
pub fn view_of(snapshot: &Snapshot) -> ViewDocument {
    let blooms = snapshot
        .blooms
        .values()
        .map(|record| {
            let members = record
                .spec
                .members()
                .iter()
                .map(|member| MemberView {
                    workpiece: member.workpiece.clone(),
                    scope_revision: member.scope_revision,
                    approval: member.approval.clone(),
                    resolution: record.claims.iter().find(|c| c.workpiece == member.workpiece).cloned(),
                })
                .collect();
            BloomView { id: record.spec.id(), status: record.status, superseded_by: record.superseded_by, members }
        })
        .collect();
    ViewDocument { mainline: snapshot.mainline, blooms }
}
