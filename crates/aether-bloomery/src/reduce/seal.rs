//! The two admission doors: sealing a draft into an active bloom, and
//! superseding a predecessor with a successor that inherits its claims
//! (ADR-0149 §The bloom). Both run the same per-member admission.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use super::{
    BloomStatus, Decision, Decisions, Outcome, SealConflict, SealError, Snapshot, StageProgress, SupersedeError,
};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::values::{BloomSpec, EvidenceKind, Membership, StageCatalog, Transformation};

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

pub(super) fn reduce_seal(snapshot: &Snapshot, spec: &BloomSpec) -> Decisions {
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

pub(super) fn reduce_supersede(snapshot: &Snapshot, predecessor: &BloomId, successor: &BloomSpec) -> Decisions {
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
    // Every successor member that does not arrive already integrated (an
    // inherited claim above) enters the line fresh: seed its cursor at the
    // entry stage and dispatch its first attempt against the successor's
    // sealed base — the same entry dispatch a seal runs (#3663). Net-new
    // members, scope-changed members, and re-admitted members whose
    // predecessor never integrated (the wedged-member escape hatch) would
    // otherwise be claimed but never executed, leaving the successor
    // unresolvable.
    for member in successor.members() {
        let inherited =
            record.claims.get(&member.workpiece).is_some_and(|claim| claim.scope_revision == member.scope_revision);
        if !inherited {
            effects.extend(entry_dispatch_effects(successor_id, member, successor.base()));
        }
    }
    effects.push(Decision::MarkSuperseded { bloom: *predecessor, by: successor_id });
    Decisions { outcome: Outcome::Superseded { predecessor: *predecessor, successor: successor_id }, effects }
}
