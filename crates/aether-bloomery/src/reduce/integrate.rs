//! Integration and resolution: recording one member's resolution claim, and
//! folding a complete claim set into the bloom's one artifact (ADR-0152).

use alloc::vec::Vec;

use super::aggregate_verify::{aggregate_review_dispatch, aggregate_verify_dispatch, at_park_ceiling};
use super::readiness::newly_ready_entries;
use super::verify_memo::{proof_of, reuse_of};
use super::{
    BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration, IntegrateError, Outcome, ResolveError, Snapshot,
};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::ids::StageId;
use crate::values::{CandidateRef, MemberCandidate, ResolutionClaim};

/// The effects one member's resolution claim produces: the claim itself, the
/// `provenance` note for the verdict it carries, and — when it completes the
/// bloom's claim set — the integration fold.
///
/// Shared with the member-`Verify` memo hit (#4891), which lands a member on the
/// same claim a dispatched pass would and differs only in that note: a fresh
/// verdict files a proof of the tree it judged, a reused one files the receipt
/// naming the proof it stood on.
///
/// The claim that completes the set dispatches integration (ADR-0152
/// §Resolution drives integration): with every member now carrying a resolution,
/// the host reactor folds each claimed candidate tree onto the integration
/// branch in member order and admits the resulting `Fact::Resolve`. The snapshot
/// has not folded this claim yet, so the completeness check counts it alongside
/// the recorded ones.
pub(super) fn claim_effects(
    snapshot: &Snapshot,
    record: &BloomRecord,
    bloom: BloomId,
    claim: &ResolutionClaim,
    provenance: Option<Decision>,
) -> Vec<Decision> {
    let mut effects = alloc::vec![Decision::RecordResolution { bloom, claim: claim.clone() }];
    effects.extend(provenance);
    // The capture commit is vehicle state, not claim identity: only a cursor
    // whose tree is this claim records one. A checkout digest sitting on the
    // claim must not be treated as a match (#5079).
    let vehicle = matching_vehicle(record, claim);
    if let Some(vehicle) = vehicle {
        effects.push(Decision::RecordCandidateVehicle { bloom, workpiece: claim.workpiece.clone(), vehicle });
    }
    // Dependents whose last unresolved edge was this claim now enter Construct.
    // Journaled on this row so replay recovers the schedule (ADR-0190 / ADR-0196).
    // The snapshot has not folded the vehicle yet, so the splice takes the
    // capture (or the claimed tree, when none was recorded) as an argument.
    effects.extend(newly_ready_entries(
        record,
        bloom,
        &claim.workpiece,
        vehicle.map_or(claim.candidate, |candidate| candidate.checkout),
    ));

    let complete = record
        .spec
        .members()
        .iter()
        .all(|member| member.workpiece == claim.workpiece || record.claims.contains_key(&member.workpiece));
    if complete {
        let members: Vec<MemberCandidate> = record
            .spec
            .members()
            .iter()
            .filter_map(|member| {
                let candidate = if member.workpiece == claim.workpiece {
                    Some(claim.candidate)
                } else {
                    record.claims.get(&member.workpiece).map(|recorded| recorded.candidate)
                };
                candidate.map(|candidate| MemberCandidate { workpiece: member.workpiece.clone(), candidate })
            })
            .collect();
        let adopt_from = adoption_source(snapshot, bloom, &members);
        effects.push(Decision::DispatchIntegration { bloom, base: record.spec.base(), members, adopt_from });
    }
    effects
}

/// The predecessor whose candidate refs this fold has to adopt before it can
/// merge them, or `None` when every folded candidate was produced under this
/// bloom's own id.
///
/// A candidate ref is addressed under the bloom that produced it, so a member
/// arriving on an inherited claim has no ref in this bloom's namespace to merge.
/// `reduce_supersede` already handles the successor whose members were *all*
/// inherited — no claim arrives to complete its set, so it dispatches its own
/// fold naming the predecessor. The set that arrives here is the mixed one: some
/// members re-ran under the successor and captured their own refs, and the claim
/// completing the set is one of theirs. Dispatched with no predecessor, that fold
/// merges a ref that only the predecessor's namespace holds, and the source
/// answers 404 every tick forever (#4903).
///
/// The lineage is read off the projection rather than carried alongside it: the
/// predecessor is the bloom this one superseded, which is the record holding
/// `superseded_by == bloom` (unique — a supersession refuses a successor id that
/// collides with a known bloom), and the claim it left behind names the
/// candidate. A member whose folded candidate is the one the predecessor
/// recorded is a member whose ref was written under the predecessor; a member
/// that re-ran carries a different candidate and its own ref. One scan of the
/// bloom map per completed claim set, over state the journal already rebuilds.
///
/// The distinction stays a *fold-wide* fact rather than a per-member one because
/// presence is the honest test and it lives at the ref: adoption is
/// adopt-if-absent, so naming the predecessor asks the source to fill in the
/// refs this bloom lacks and to leave every ref it already carries — including a
/// successor-produced capture standing beside an inherited one — exactly where
/// it is.
/// The cursor's [`CandidateRef`] when its tree is this claim's identity.
///
/// Matching on `tree` — never `checkout` — is what keeps a capture commit
/// from substituting for the claimed tree (ADR-0152 / #5079).
fn matching_vehicle(record: &BloomRecord, claim: &ResolutionClaim) -> Option<CandidateRef> {
    record
        .progress
        .get(&claim.workpiece)
        .and_then(|progress| progress.candidate)
        .filter(|candidate| candidate.tree == claim.candidate)
}

fn adoption_source(snapshot: &Snapshot, bloom: BloomId, members: &[MemberCandidate]) -> Option<BloomId> {
    let (predecessor, record) = snapshot.blooms.iter().find(|(_, record)| record.superseded_by == Some(bloom))?;
    let inherited = members
        .iter()
        .any(|member| record.claims.get(&member.workpiece).is_some_and(|claim| claim.candidate == member.candidate));

    inherited.then_some(*predecessor)
}

pub(super) fn reduce_integrate(snapshot: &Snapshot, bloom: &BloomId, claim: &ResolutionClaim) -> Decisions {
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

    // A member's passing terminal `Verify` arrives here and nowhere else, so
    // this is where the line learns that a tree passed its gates (#4891) — the
    // fold of a single member is that same tree, and a repair lap can hand it
    // back unchanged.
    let effects = claim_effects(snapshot, record, *bloom, claim, proof_of(*bloom, StageId::Verify, &claim.evidence));

    Decisions { outcome: Outcome::Integrated { bloom: *bloom, workpiece: claim.workpiece.clone() }, effects }
}
pub(super) fn reduce_resolve(
    snapshot: &Snapshot,
    bloom: &BloomId,
    tree: &Digest,
    head: &Digest,
    lineage: &[Digest],
) -> Decisions {
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
    if let Some(member) = record.spec.members().iter().find(|member| !record.claims.contains_key(&member.workpiece)) {
        return Decisions::rejected(Outcome::ResolveRejected(ResolveError::MemberNotIntegrated {
            workpiece: member.workpiece.clone(),
        }));
    }
    // The claim set checked out, so the fold's head is gated before the bloom
    // may resolve (ADR-0153): hold the fold on the record and dispatch the
    // whole-bloom aggregate *verify* against it — the claim scan above stays
    // the integrity gate, the compiler is the mechanical gate, and the critic
    // the judgment gate that a passing verify hands off to. Verify runs first
    // because it is the cheaper and more decisive of the two, and there is
    // nothing for a critic to judge in a fold that does not build. The ceiling
    // is the same inclusive park comparison the verify completion gate uses:
    // a fold whose next roll is at or past AggregateVerify's catalog budget
    // is refused fail-closed (unreachable through this reducer — a wedged
    // bloom's members stay closed, so no re-fold dispatches — but a buggy
    // reactor must not buy a roll the vocabulary forbids).
    let roll = record.aggregate_verify_rolls + 1;
    if at_park_ceiling(record, StageId::AggregateVerify, roll) {
        return Decisions::rejected(Outcome::ResolveRejected(ResolveError::ReviewCeiling {
            rolls: record.aggregate_verify_rolls,
        }));
    }
    let integration = FoldedIntegration { tree: *tree, head: *head, lineage: lineage.to_vec() };
    let hold = Decision::RecordIntegration { bloom: *bloom, integration: Some(integration) };

    // The fold may be a tree this bloom has already proven (#4891): a
    // single-member fold is byte-identical to the candidate its member verified,
    // so the mechanical gate would re-run for a verdict the journal holds. Pass
    // by identity and hand the same fold straight to the critic, exactly as a
    // returning green verdict would. A multi-member fold is a tree that never
    // existed before this moment, so it misses and the gates run.
    if let Some(proof) = record.verify_proof_for(*tree) {
        return Decisions {
            outcome: Outcome::AggregateVerifyReused { bloom: *bloom, rolls: roll, proof: proof.evidence.detail },
            effects: alloc::vec![
                hold,
                reuse_of(*bloom, StageId::AggregateVerify, proof),
                Decision::RecordAggregateVerifyRoll { bloom: *bloom, rolls: roll },
                aggregate_review_dispatch(record, *bloom, *tree, *head),
            ],
        };
    }

    Decisions {
        outcome: Outcome::AggregateVerifyDispatched { bloom: *bloom, roll },
        effects: alloc::vec![hold, aggregate_verify_dispatch(record, *bloom, *tree, *head)],
    }
}
