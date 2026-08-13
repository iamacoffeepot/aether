//! Integration and resolution: recording one member's resolution claim, and
//! folding a complete claim set into the bloom's one artifact (ADR-0152).

use alloc::vec::Vec;

use super::aggregate_verify::aggregate_review_dispatch;
use super::attempt::stage_binding;
use super::verify_memo::{proof_of, reuse_of};
use super::{
    BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration, IntegrateError, Outcome, ResolveError, Snapshot,
};
use crate::digest::Digest;
use crate::ids::BloomId;
use crate::ids::StageId;
use crate::values::{MemberCandidate, ResolutionClaim, Transformation};

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
    record: &BloomRecord,
    bloom: BloomId,
    claim: &ResolutionClaim,
    provenance: Option<Decision>,
) -> Vec<Decision> {
    let mut effects = alloc::vec![Decision::RecordResolution { bloom, claim: claim.clone() }];
    effects.extend(provenance);

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
            bloom,
            base: record.spec.base(),
            members,
            // Its own members produced these refs under its own id.
            adopt_from: None,
        });
    }
    effects
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
    let effects = claim_effects(record, *bloom, claim, proof_of(*bloom, StageId::Verify, &claim.evidence));

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
    // is AggregateVerify's catalog retry budget over the record's
    // consumed-verdict count; a fold arriving past it is refused fail-closed
    // (unreachable through this reducer — a wedged bloom's members stay
    // closed, so no re-fold dispatches — but a buggy reactor must not buy a
    // roll the vocabulary forbids).
    let roll = record.aggregate_verify_rolls + 1;
    if roll > record.stage_catalog.retry_budget_of(StageId::AggregateVerify).unwrap_or(1) {
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

    let binding = stage_binding(&record.stage_catalog, StageId::AggregateVerify);

    Decisions {
        outcome: Outcome::AggregateVerifyDispatched { bloom: *bloom, roll },
        effects: alloc::vec![
            hold,
            Decision::DispatchAggregateVerify {
                bloom: *bloom,
                transformation: Transformation::for_aggregate_verify(&binding, *tree, *head),
                roll,
                profile: binding.profile,
            },
        ],
    }
}
