//! The whole-bloom aggregate verify: the mechanical gate over the folded head,
//! run before the critic ever sees it.
//!
//! Every member verified its own candidate in isolation and passed. The fold is
//! the first tree that carries all of them at once, so it is the first thing
//! that can fail on their interaction — two members that each compile and
//! together do not. Without this gate the landing CI is what discovers that,
//! downstream of the point where the bloom can still route it back to an owner.

use alloc::vec::Vec;

use super::attempt::stage_profile;
use super::review::reenter_members;
use super::{AggregateVerifyError, BloomStatus, Decision, Decisions, Outcome, Snapshot};
use crate::ids::{BloomId, StageId};
use crate::values::{Evidence, Transformation};

/// Reduce a whole-bloom aggregate-verify verdict.
///
/// A passing verdict hands the same fold to the aggregate review — the fold
/// builds, so it is now worth a critic's time. A failing one re-opens every
/// member into the repair-only `Refine` and clears the stale fold, until the
/// stage's own catalog budget is spent, at which point the bloom parks to the
/// owner rather than re-folding a combination that has not built yet.
///
/// Every member re-opens, with no implication to narrow it: a compile failure
/// over the fold belongs to the combination, not to a member that passed on its
/// own, and over-routing is the fail-closed direction — the same principle the
/// review applies to an empty implication.
pub(super) fn reduce_aggregate_verify_completed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    passed: bool,
    evidence: &Evidence,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::AggregateVerifyRejected(AggregateVerifyError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::AggregateVerifyRejected(AggregateVerifyError::UnknownOrInactiveBloom));
    }
    let Some(integration) = record.integration.clone() else {
        return Decisions::rejected(Outcome::AggregateVerifyRejected(AggregateVerifyError::NoPendingIntegration));
    };
    // The verdict must bind the exact tree the held fold produced — a stale
    // verdict from a superseded fold cannot act on a newer integration.
    if !evidence.validates(&integration.tree) {
        return Decisions::rejected(Outcome::AggregateVerifyRejected(AggregateVerifyError::SubjectMismatch {
            expected: integration.tree,
            got: evidence.subject,
        }));
    }

    let rolls = record.aggregate_verify_rolls + 1;
    let mut effects = alloc::vec![
        Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() },
        Decision::RecordAggregateVerifyRoll { bloom: *bloom, rolls },
    ];

    if passed {
        // The fold stays held: the review judges the same integration this
        // verify just built, and it is the passing review that consumes it.
        effects.push(Decision::DispatchAggregateReview {
            bloom: *bloom,
            transformation: Transformation::for_aggregate_review(integration.tree, integration.head),
            roll: record.aggregate_rolls + 1,
            profile: stage_profile(&record.stage_catalog, StageId::AggregateReview),
        });
        return Decisions { outcome: Outcome::AggregateVerifyPassed { bloom: *bloom, rolls }, effects };
    }

    if rolls >= record.stage_catalog.retry_budget_of(StageId::AggregateVerify).unwrap_or(1) {
        // The budget is spent on a fold that still does not build. The fold
        // stays held as the owner's decision context and no member re-opens —
        // the same bloom-scope park the review's ceiling raises, so an adopting
        // answer that names the question re-arms the cycle.
        effects.push(Decision::RecordReviewPark { bloom: *bloom, question: Some(evidence.detail) });
        return Decisions {
            outcome: Outcome::AggregateVerifyParked { bloom: *bloom, rolls, question: evidence.detail },
            effects,
        };
    }

    let members: Vec<_> = record.spec.members().iter().map(|member| member.workpiece.clone()).collect();
    effects.extend(reenter_members(record, bloom, &members, integration.tree));

    Decisions { outcome: Outcome::AggregateVerifyReentered { bloom: *bloom, members, rolls }, effects }
}
