//! The whole-bloom aggregate review: one verdict over the integrated head,
//! bounded by the two-pass ceiling that parks the bloom to the owner rather
//! than buying a third roll (ADR-0153).

use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects};
use super::{AggregateReviewError, BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{ConfigRegistry, Evidence, ResolvedBloom};

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
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::AggregateReviewRejected(AggregateReviewError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::AggregateReviewRejected(AggregateReviewError::UnknownOrInactiveBloom));
    }
    let Some(integration) = record.integration.clone() else {
        return Decisions::rejected(Outcome::AggregateReviewRejected(AggregateReviewError::NoPendingIntegration));
    };
    // The verdict must bind the exact tree the held fold produced — a stale
    // verdict from a superseded fold cannot act on a newer integration.
    if !evidence.validates(&integration.tree) {
        return Decisions::rejected(Outcome::AggregateReviewRejected(AggregateReviewError::SubjectMismatch {
            expected: integration.tree,
            got: evidence.subject,
        }));
    }
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
    record: &super::BloomRecord,
    bloom: &BloomId,
    members: &[WorkpieceId],
    fold: Digest,
) -> Vec<Decision> {
    let mut effects = alloc::vec![Decision::RecordIntegration { bloom: *bloom, integration: None }];
    for workpiece in members {
        effects.push(Decision::RevokeResolution { bloom: *bloom, workpiece: workpiece.clone() });
        let member = record.spec.members().iter().find(|member| member.workpiece == *workpiece);
        let candidate = record.claims.get(workpiece).map(|claim| claim.candidate);
        let cursor = record.progress.get(workpiece).copied();
        let progress = StageProgress {
            stage: StageId::Refine,
            attempts: 1,
            candidate: cursor.and_then(|progress| progress.candidate),
            repair_rolls: cursor.map_or(0, |progress| progress.repair_rolls),
        };
        // The dispatch targets re-resolve like a member-line move (ADR-0152):
        // the claimed candidate tree binds the evidence and its capture commit
        // is the checkout; the cursor's candidate carries the checkout pair.
        let (subject, checkout) = progress.candidate.map_or_else(
            || (candidate.or_else(|| member.map(|m| m.scope_revision)).unwrap_or(fold), record.spec.base()),
            |current| (current.tree, current.checkout),
        );
        effects.extend(move_effects(
            *bloom,
            workpiece,
            member.map_or(fold, |m| m.scope_revision),
            progress,
            DispatchTargets { subject, checkout },
            SealedLine {
                configs: member.map_or_else(ConfigRegistry::default, |m| m.configs.layered_over(record.spec.configs())),
                catalog: &record.stage_catalog,
            },
        ));
    }
    effects
}
