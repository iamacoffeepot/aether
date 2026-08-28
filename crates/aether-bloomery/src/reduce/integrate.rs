//! Integration and resolution: recording one member's resolution claim, and
//! folding a complete claim set into the bloom's one artifact (ADR-0152).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::aggregate_verify::{aggregate_gate_dispatches, aggregate_review_dispatch, at_park_ceiling};
use super::boundary::EventBoundary;
use super::gate::AGGREGATE_VERIFY_GATE;
use super::lease::resume_entries;
use super::readiness::newly_ready_entries;
use super::verify_memo::{proof_of, reuse_of};
use super::{
    BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration, IntegrateError, Outcome, ResolveError, Snapshot,
};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::ids::StageId;
use crate::reads;
use crate::values::{CandidateRef, MemberCandidate, ResolutionClaim, VerifyProof};

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

    // Members this one evicted off a contended file re-dispatch on the base it
    // just advanced to (ADR-0204). Journaled on this row for the same reason
    // the readiness entries above are: replay folds decisions, so a resume the
    // integration decided has to be recorded by it.
    effects.extend(resume_entries(
        record,
        bloom,
        &claim.workpiece,
        vehicle.map_or(claim.candidate, |candidate| candidate.checkout),
        snapshot.lease_evictions.get(&bloom),
    ));

    // A withdrawn member never produces a claim and contributes no candidate
    // (#5327), so the three folds that are otherwise total over the sealed
    // member list skip it. Without the skip one withdrawn member pins the
    // bloom, its siblings' finished work, and the mainline behind it.
    let complete = record
        .spec
        .members()
        .iter()
        .filter(|member| !record.withdrawn.contains_key(&member.workpiece))
        .all(|member| member.workpiece == claim.workpiece || record.claims.contains_key(&member.workpiece));
    if complete {
        let members: Vec<MemberCandidate> = record
            .spec
            .members()
            .iter()
            .filter(|member| !record.withdrawn.contains_key(&member.workpiece))
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

pub(super) fn adoption_source(snapshot: &Snapshot, bloom: BloomId, members: &[MemberCandidate]) -> Option<BloomId> {
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

/// The `aggregate_verify` boundary (ADR-0206): a complete claim set is folded
/// and handed to the composite gates, or the record says which guard stopped it.
///
/// The unknown-bloom lookup stays an ordinary early return — an addressing
/// error has no record to file a refusal against.
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
    // The roll this fold would buy, read before the guards so the ceiling guard
    // and the effects below agree on one number.
    let roll = record.aggregate_verify_rolls + 1;
    let pending = || record.holds.iter().next().copied();
    let unclaimed = || {
        record
            .spec
            .members()
            .iter()
            .filter(|member| !record.withdrawn.contains_key(&member.workpiece))
            .find(|member| !record.claims.contains_key(&member.workpiece))
            .map(|member| member.workpiece.clone())
    };

    EventBoundary::new(AGGREGATE_VERIFY_GATE, *bloom)
        .require(
            "bloom_sealed",
            || record.status == BloomStatus::Sealed,
            || reads![status: format!("{:?}", record.status), required: "Sealed"],
            || Outcome::ResolveRejected(ResolveError::UnknownOrInactiveBloom),
        )
        // A member held on a parked question cannot integrate, so a bloom with
        // any open hold cannot resolve (ADR-0151) — guarded before the
        // per-member claim scan so the pending decision is the named reason,
        // not a bare MemberNotIntegrated.
        .require(
            "no_open_question",
            || pending().is_none(),
            || reads![questions: record.holds.len()],
            || {
                pending().map_or(Outcome::ResolveRejected(ResolveError::UnknownOrInactiveBloom), |question| {
                    Outcome::ResolveRejected(ResolveError::PendingDecision { question })
                })
            },
        )
        // Every frozen member must carry a resolution claim before the bloom
        // can resolve — a resolved bloom carries a claim for every member
        // (ADR-0149 §The bloom).
        .require(
            "every_member_claimed",
            || unclaimed().is_none(),
            || {
                reads![
                    unclaimed: unclaimed().map_or_else(String::new, |workpiece| workpiece.0),
                    claimed: record.claims.len(),
                ]
            },
            || {
                unclaimed().map_or(Outcome::ResolveRejected(ResolveError::UnknownOrInactiveBloom), |workpiece| {
                    Outcome::ResolveRejected(ResolveError::MemberNotIntegrated { workpiece })
                })
            },
        )
        // The ceiling is the same inclusive park comparison the verify
        // completion gate uses: a fold whose next roll is at or past
        // AggregateVerify's catalog budget is refused fail-closed (unreachable
        // through this reducer — a wedged bloom's members stay closed, so no
        // re-fold dispatches — but a buggy reactor must not buy a roll the
        // vocabulary forbids).
        .require(
            "under_verify_budget",
            || !at_park_ceiling(record, StageId::AggregateVerify, roll),
            || reads![roll: roll, spent: record.aggregate_verify_rolls],
            || Outcome::ResolveRejected(ResolveError::ReviewCeiling { rolls: record.aggregate_verify_rolls }),
        )
        .decide(|| folded(record, *bloom, *tree, *head, lineage, roll))
}

/// The effects a passing fold produces: the fold is held on the record, and
/// both composite gates are dispatched against it — the claim scan above stays
/// the integrity gate, the compiler is the mechanical gate, and the critic the
/// judgment gate. They run together rather than in series because neither reads
/// the other's verdict, so the bloom pays the larger of the two latencies
/// instead of their sum; the landing waits on the join of the two passes
/// (`BloomRecord::aggregate_passed`), and a refusal from either re-weaves the
/// composition once.
fn folded(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
    lineage: &[Digest],
    roll: u32,
) -> Decisions {
    let integration = FoldedIntegration { tree, head, lineage: lineage.to_vec() };
    let hold = Decision::RecordIntegration { bloom, integration: Some(integration) };

    // The fold may be a tree this bloom's fold gates have already proven
    // (#4891) — a re-weave that reproduces a tree an earlier round already put
    // through them. Pass by identity: the mechanical half of the join is
    // recorded straight away, exactly as a returning green verdict would record
    // it, and only the critic is dispatched.
    //
    // A member's own proof is not an answer here, however byte-identical the
    // fold is to that member's candidate. The member position does not run
    // `verify.docs`, so its green says nothing about the gate that most needs a
    // whole tree — and a fold that passed on it would carry that silence into
    // the base receipt a landing mints.
    if let Some(proof) = record.verify_proof_for(StageId::AggregateVerify, tree) {
        return Decisions {
            outcome: Outcome::AggregateVerifyReused { bloom, rolls: roll, proof: proof.evidence.detail },
            effects: reused_fold_effects(record, bloom, tree, head, hold, proof, roll),
        };
    }

    let mut effects = alloc::vec![hold];
    effects.extend(aggregate_gate_dispatches(record, bloom, tree, head));

    Decisions { outcome: Outcome::AggregateVerifyDispatched { bloom, roll }, effects }
}

/// The effects of a fold whose mechanical verdict the journal already holds.
///
/// The gate-pass row sits after the `hold`, which clears the join: the fold
/// being recorded here is the subject this pass is about. Only the critic is
/// dispatched, and the brake still gets to withhold that order — which is why
/// the review dispatch is extended in rather than pushed.
fn reused_fold_effects(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
    hold: Decision,
    proof: &VerifyProof,
    roll: u32,
) -> Vec<Decision> {
    let mut effects = alloc::vec![
        hold,
        reuse_of(bloom, StageId::AggregateVerify, proof),
        Decision::RecordAggregateVerifyRoll { bloom, rolls: roll },
        Decision::RecordAggregateGatePass { bloom, stage: StageId::AggregateVerify },
    ];
    effects.extend(aggregate_review_dispatch(record, bloom, tree, head));
    effects
}
