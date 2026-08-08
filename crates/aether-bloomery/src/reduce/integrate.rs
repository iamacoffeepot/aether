//! Integration and resolution: recording one member's resolution claim, and
//! folding a complete claim set into the bloom's one artifact (ADR-0152).

use super::attempt::stage_profile;
use super::{BloomStatus, Decision, Decisions, FoldedIntegration, IntegrateError, Outcome, ResolveError, Snapshot};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::ids::StageId;
use crate::values::{MemberCandidate, ResolutionClaim, Transformation};

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
    let mut effects = alloc::vec![Decision::RecordResolution { bloom: *bloom, claim: claim.clone() }];
    // The claim that completes the set dispatches integration (ADR-0152
    // §Resolution drives integration): with every member now carrying a
    // resolution, the host reactor folds each claimed candidate tree onto the
    // integration branch in member order and admits the resulting
    // `Fact::Resolve`. The snapshot has not folded this claim yet, so the
    // completeness check counts it alongside the recorded ones.
    let complete = record
        .spec
        .members()
        .iter()
        .all(|member| member.workpiece == claim.workpiece || record.claims.contains_key(&member.workpiece));
    if complete {
        let members = record
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
        effects.push(Decision::DispatchIntegration {
            bloom: *bloom,
            base: record.spec.base(),
            members,
            // Its own members produced these refs under its own id.
            adopt_from: None,
        });
    }
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
    // The claim set checked out, so the fold's head is judged before the bloom
    // may resolve (ADR-0153): hold the fold on the record and dispatch the
    // whole-bloom aggregate review against it — the claim scan above stays the
    // integrity gate, the review is the judgment gate. The
    // ceiling is AggregateReview's catalog retry budget over the record's
    // consumed-verdict count; a fold arriving past it is refused fail-closed
    // (unreachable through this reducer — a wedged bloom's members stay
    // closed, so no re-fold dispatches — but a buggy reactor must not buy a
    // roll the vocabulary forbids).
    let roll = record.aggregate_rolls + 1;
    if roll > record.stage_catalog.retry_budget_of(StageId::AggregateReview).unwrap_or(1) {
        return Decisions::rejected(Outcome::ResolveRejected(ResolveError::ReviewCeiling {
            rolls: record.aggregate_rolls,
        }));
    }
    let integration = FoldedIntegration { tree: *tree, head: *head, lineage: lineage.to_vec() };
    Decisions {
        outcome: Outcome::AggregateReviewDispatched { bloom: *bloom, roll },
        effects: alloc::vec![
            Decision::RecordIntegration { bloom: *bloom, integration: Some(integration) },
            Decision::DispatchAggregateReview {
                bloom: *bloom,
                transformation: Transformation::for_aggregate_review(*tree, *head),
                roll,
                profile: stage_profile(&record.stage_catalog, StageId::AggregateReview),
            },
        ],
    }
}
