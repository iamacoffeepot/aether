//! The whole-bloom aggregate review: one verdict over the integrated head,
//! bounded by the two-pass ceiling that parks the bloom to the owner rather
//! than buying a third roll (ADR-0153).

use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate, stage_binding};
use super::{
    AggregateReviewError, AggregateReviewFault, BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration,
    Outcome, Snapshot, StageProgress,
};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{ConfigRegistry, Evidence, ResolvedBloom, Transformation, VerifyFailureSet};

/// The bloom record and the integration fold a fold-bound aggregate-review
/// result may act on, or the refusal it earns.
///
/// The three refusals every aggregate-review result makes, in one place: an
/// unknown or inactive bloom, no held integration, and evidence naming a tree
/// other than the held fold's. The last is the load-bearing one — a stale result
/// from a superseded fold must not act on a newer integration, whether it
/// carries a verdict or an executor fault.
fn held_fold_under_review<'a>(
    snapshot: &'a Snapshot,
    bloom: &BloomId,
    evidence: &Evidence,
) -> Result<(&'a BloomRecord, FoldedIntegration), AggregateReviewError> {
    let record = snapshot
        .blooms
        .get(bloom)
        .filter(|record| record.status == BloomStatus::Sealed)
        .ok_or(AggregateReviewError::UnknownOrInactiveBloom)?;
    let integration = record.integration.clone().ok_or(AggregateReviewError::NoPendingIntegration)?;
    if !evidence.validates(&integration.tree) {
        return Err(AggregateReviewError::SubjectMismatch { expected: integration.tree, got: evidence.subject });
    }
    Ok((record, integration))
}

/// Reduce a whole-bloom aggregate-review verdict (ADR-0153). A passing verdict
/// resolves the bloom from its held fold — [`Decision::SetResolved`] plus the
/// [`Decision::DispatchLand`] the land reactor consumes. A failing verdict
/// freezes into member routing: every implicated member's claim is revoked and
/// its cursor re-enters the repair-only `Refine` (the host threads the
/// findings slice onto the dispatch), the stale fold is cleared, and the
/// re-fold that follows re-integration dispatches the delta-confirm. The
/// second failing verdict parks the bloom to the owner — the two-pass
/// ceiling; the machine never buys a third roll, though an adopting answer
/// lets the owner buy a fresh cycle.
pub(super) fn reduce_aggregate_review_completed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    passed: bool,
    evidence: &Evidence,
    implicated: &[WorkpieceId],
) -> Decisions {
    let (record, integration) = match held_fold_under_review(snapshot, bloom, evidence) {
        Ok(held) => held,
        Err(refusal) => return Decisions::rejected(Outcome::AggregateReviewRejected(refusal)),
    };
    let rolls = record.aggregate_rolls + 1;
    let mut effects = alloc::vec![
        Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() },
        Decision::RecordAggregateRoll { bloom: *bloom, rolls },
    ];
    if passed {
        let resolution_claims = record.claims.values().cloned().collect::<Vec<_>>();
        let resolved = ResolvedBloom {
            bloom: *bloom,
            tree: integration.tree,
            head: integration.head,
            lineage: integration.lineage.clone(),
            resolution_claims,
        };
        // Resolution is land-readiness: the bloom now carries its one judged
        // artifact and a claim for every member, so the source-port CAS land
        // can be driven (ADR-0149 migration step 3). `new_head` is the
        // integrated head commit's digest (distinct from the artifact `tree`),
        // the head mainline advances to; the reducer never does the I/O. The
        // consumed fold is cleared — a resolved bloom holds no pending review.
        effects.push(Decision::RecordIntegration { bloom: *bloom, integration: None });
        effects.push(Decision::SetResolved { bloom: *bloom, resolved: resolved.clone() });
        effects.push(Decision::DispatchLand {
            bloom: *bloom,
            expected_base: record.spec.base(),
            new_head: integration.head,
        });
        return Decisions { outcome: Outcome::Resolved(resolved), effects };
    }
    // A failing verdict routes to owners. A named non-member is malformed —
    // validated before any effect applies, so such a verdict changes nothing.
    // An *empty* implication routes to every member: the host admits the
    // verdict without membership knowledge (only the reducer holds the sealed
    // set), and over-routing is the fail-closed direction — a failing verdict
    // must never strand for want of an owner. The findings decomposition
    // (ADR-0153 §Findings freeze) narrows the set where ownership is parsed.
    if let Some(stranger) =
        implicated.iter().find(|wp| !record.spec.members().iter().any(|member| member.workpiece == **wp))
    {
        return Decisions::rejected(Outcome::AggregateReviewRejected(AggregateReviewError::NotAMember(
            stranger.clone(),
        )));
    }
    let implicated: Vec<WorkpieceId> = if implicated.is_empty() {
        record.spec.members().iter().map(|member| member.workpiece.clone()).collect()
    } else {
        implicated.to_vec()
    };
    if rolls >= record.stage_catalog.retry_budget_of(StageId::AggregateReview).unwrap_or(1) {
        // The delta-confirm still failed: the two-pass ceiling parks the bloom
        // to the owner (ADR-0151's hold vocabulary at bloom scope). The fold
        // stays held (the owner's decision context), no member re-opens, no
        // further review dispatches; the failing review's record artifact is
        // the parked question an adopting answer must name to re-arm the
        // cycle.
        effects.push(Decision::RecordReviewPark { bloom: *bloom, question: Some(evidence.detail) });
        return Decisions {
            outcome: Outcome::AggregateReviewParked { bloom: *bloom, rolls, question: evidence.detail },
            effects,
        };
    }
    // First failing verdict: re-open every implicated member — revoke its
    // claim and route it into the repair-only Refine against its own claimed
    // candidate (the fold's tree is the whole bloom's, never one member's
    // repair target). The cleared fold marks the integration stale; when the
    // re-opened members re-integrate, the completing claim re-dispatches the
    // fold and the fresh head gets the delta-confirm.
    effects.extend(reenter_members(record, bloom, &implicated, integration.tree));

    Decisions { outcome: Outcome::AggregateReviewReentered { bloom: *bloom, members: implicated, rolls }, effects }
}

/// Reduce an aggregate-review executor environment fault (ADR-0176) — the
/// dispatched review reporting that it could not judge the fold at all.
///
/// A branch entirely separate from [`reduce_aggregate_review_completed`],
/// because nothing here is a verdict about a candidate: the fold stays held,
/// every member keeps its claim and its cursor, and no findings are written. The
/// fault records against the held fold and, while the sealed `AggregateReview`
/// budget allows, redispatches the *same* tree and head under a fresh order.
/// At the ceiling it emits no dispatch — the folded record is the terminal
/// bloom-scoped wedge an operator reads, and recovery is an explicit successor
/// once the environment is repaired, never a reactor poll that quietly buys
/// another attempt.
///
/// Refused on exactly the three axes a fold-bound aggregate verdict carrying no
/// implication can be refused on: an unknown or inactive bloom, no held
/// integration, and a subject that is not the held fold's tree.
pub(super) fn reduce_aggregate_review_executor_fault(
    snapshot: &Snapshot,
    bloom: &BloomId,
    evidence: &Evidence,
) -> Decisions {
    let (record, integration) = match held_fold_under_review(snapshot, bloom, evidence) {
        Ok(held) => held,
        Err(refusal) => return Decisions::rejected(Outcome::AggregateReviewRejected(refusal)),
    };

    // The same rule the `RecordEvidence` fold applies, read here so the ceiling
    // decides against the count the record will actually reach.
    let fault = AggregateReviewFault::next(record.aggregate_fault.as_ref(), integration.tree, evidence.detail);
    let budget = record.stage_catalog.retry_budget_of(StageId::AggregateReview).unwrap_or(1);
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];

    if fault.rolls >= budget {
        return Decisions { outcome: Outcome::AggregateReviewExecutorWedged { bloom: *bloom, fault, budget }, effects };
    }

    let binding = stage_binding(&record.stage_catalog, StageId::AggregateReview);
    // The same held tree and head, under a fresh order: the fold was never
    // judged, so re-running the review is the whole retry — not a re-fold, and
    // not a member lap. The roll stays the critic's own unspent cursor.
    effects.push(Decision::DispatchAggregateReview {
        bloom: *bloom,
        transformation: Transformation::for_aggregate_review(
            &binding,
            integration.tree,
            integration.head,
            record.spec.base(),
        ),
        roll: record.aggregate_rolls + 1,
        profile: binding.profile,
        configs: record.spec.configs().clone(),
    });

    Decisions { outcome: Outcome::AggregateReviewExecutorFaulted { bloom: *bloom, fault, budget }, effects }
}

/// Clear the stale fold and route every named member back into the repair-only
/// `Refine` against its own claimed candidate.
///
/// Shared by both aggregate gates because the routing is the same regardless of
/// which one refused the fold: the claim is revoked (a bloom with a revoked
/// claim cannot resolve), the cursor re-enters `Refine`, and the cleared fold
/// marks the integration stale so the completing re-integration dispatches a
/// fresh one. `fold` is the fold's tree, used only as the last-resort subject
/// for a member carrying neither a cursor candidate nor a claim.
pub(super) fn reenter_members(
    record: &BloomRecord,
    bloom: &BloomId,
    members: &[WorkpieceId],
    fold: Digest,
) -> Vec<Decision> {
    let mut effects = alloc::vec![Decision::RecordIntegration { bloom: *bloom, integration: None }];
    for workpiece in members {
        effects.push(Decision::RevokeResolution { bloom: *bloom, workpiece: workpiece.clone() });
        let member = record.spec.members().iter().find(|member| member.workpiece == *workpiece);
        let claimed_candidate = record.claims.get(workpiece).map(|claim| claim.candidate);
        let cursor = record.progress.get(workpiece).copied();
        let progress = StageProgress {
            stage: StageId::Refine,
            attempts: 1,
            candidate: cursor.and_then(|progress| progress.candidate),
            repair_rolls: cursor.map_or(0, |progress| progress.repair_rolls),
            seen_verify_failures: cursor.map_or(VerifyFailureSet::EMPTY, |progress| progress.seen_verify_failures),
            fold_checkpoint: None,
            fold_conflict_evidence: None,
        };
        // The dispatch targets re-resolve like a member-line move (ADR-0152):
        // the claimed candidate tree binds the evidence and its capture commit
        // is the checkout; the cursor's candidate carries the checkout pair.
        let (subject, checkout) = progress.candidate.map_or_else(
            || (claimed_candidate.or_else(|| member.map(|m| m.scope_revision)).unwrap_or(fold), record.spec.base()),
            |current| (current.tree, current.checkout),
        );
        effects.extend(move_effects_with_candidate(
            *bloom,
            workpiece,
            member.map_or(fold, |m| m.scope_revision),
            progress,
            DispatchTargets { subject, checkout },
            progress.candidate.map(|current| current.tree).or(claimed_candidate),
            SealedLine {
                configs: member.map_or_else(ConfigRegistry::default, |m| m.configs.layered_over(record.spec.configs())),
                catalog: &record.stage_catalog,
                base: record.spec.base(),
            },
        ));
    }
    effects
}
