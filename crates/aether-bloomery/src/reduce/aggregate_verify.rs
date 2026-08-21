//! The whole-bloom aggregate verify: the mechanical gate over the folded head,
//! run beside the critic over the same fold.
//!
//! Every member verified its own candidate in isolation and passed. The fold is
//! the first tree that carries all of them at once, so it is the first thing
//! that can fail on their interaction — two members that each compile and
//! together do not. Without this gate the landing CI is what discovers that,
//! downstream of the point where the bloom can still route it back to an owner.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::attempt::stage_binding;
use super::boundary::EffectBoundary;
use super::composition::{Refusal, finding_of, reweave};
use super::gate::{AGGREGATE_REVIEW_GATE, AGGREGATE_VERIFY_GATE};
use super::verify_memo::proof_of;
use super::{AggregateVerifyError, BloomRecord, BloomStatus, Decision, Decisions, Outcome, Snapshot};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId};
use crate::reads;
use crate::values::{Evidence, Transformation};

/// Fallback when a sealed catalog binds no retry budget: one attempt, then the ceiling.
pub(super) const DEFAULT_RETRY_BUDGET: u32 = 1;

/// Whether `rolls` has reached the stage's park ceiling.
///
/// Inclusive: a roll count equal to the catalog budget parks (or refuses a new
/// fold) rather than buying another attempt. One comparison so the budget means
/// the same thing at the verify completion gate and the resolve dispatch gate.
pub(super) fn at_park_ceiling(record: &BloomRecord, stage: StageId, rolls: u32) -> bool {
    rolls >= record.stage_catalog.retry_budget_of(stage).unwrap_or(DEFAULT_RETRY_BUDGET)
}

/// The dispatch that hands a tree to the composition's `Verify` — the composite
/// gate run over `tree` / `head`.
///
/// Named once because two paths reach it: the completed fold
/// ([`super::integrate`]) and a returning weave repair ([`super::composition`]),
/// which is the same position re-entered after a repair lap. Both reach it
/// through [`aggregate_gate_dispatches`], which sends the critic out beside it.
///
/// Under an operator hold the work order is withheld and a
/// [`Decision::DeferAggregate`] is recorded instead (#5100), the same swap
/// [`super::attempt::move_effects_with_candidate`] makes for a member lap.
pub(super) fn aggregate_verify_dispatch(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
) -> Vec<Decision> {
    let roll = record.aggregate_verify_rolls + 1;

    gate_aggregate(
        record,
        bloom,
        AGGREGATE_VERIFY_GATE,
        StageId::AggregateVerify,
        owed_aggregate_verify(record, bloom, tree, head, roll),
    )
}

/// The dispatch that hands a built fold to the critic: the `AggregateReview`
/// lane over the same `tree` / `head` the mechanical gate just cleared.
///
/// Named once because several paths reach it — the fresh fold's pair
/// ([`aggregate_gate_dispatches`]), the executor-fault retry, and the park-adopt
/// re-arm — and a second copy would let them hand the critic different work
/// orders. Each retry path goes through [`gate_aggregate`] with its own roll so
/// it cannot hand the critic a different tree. Held, the work order is withheld
/// the same way [`aggregate_verify_dispatch`] withholds its own (#5100).
pub(super) fn aggregate_review_dispatch(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
) -> Vec<Decision> {
    gate_aggregate(
        record,
        bloom,
        AGGREGATE_REVIEW_GATE,
        StageId::AggregateReview,
        owed_aggregate_review(record, bloom, tree, head, record.aggregate_rolls + 1),
    )
}

/// Both composite gates over one fold, dispatched together.
///
/// The mechanical gate and the critic judge the same `tree` / `head` and share
/// nothing but their subject: the compiler does not read the critic's verdict
/// and the critic does not read the compiler's. Running them in series bought
/// no information and cost the bloom the sum of two lane latencies on every
/// fold; running them together costs the larger of the two. What the ordering
/// used to protect — never spending the paid critic lane on a fold that does
/// not build — is bounded instead by the fact that a refusal from either gate
/// re-weaves the composition once, and the join at
/// [`BloomRecord::aggregate_passed`] is what keeps a landing waiting for both.
///
/// Named once because every position that hands a *fresh* fold to the gates
/// reaches it — the completed integration and the returning weave repair — so
/// neither can dispatch half the pair. The two retry paths do not: an executor
/// fault re-runs only the gate that faulted, and an operator release re-emits
/// only the orders the hold withheld.
pub(super) fn aggregate_gate_dispatches(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
) -> Vec<Decision> {
    let mut effects = aggregate_verify_dispatch(record, bloom, tree, head);
    effects.extend(aggregate_review_dispatch(record, bloom, tree, head));
    effects
}

/// Withhold an aggregate work order while the bloom is on the operator brake
/// (#5100). The one place a [`Decision::DeferAggregate`] is built, so a later
/// site that reaches for a helper here inherits the gate.
///
/// The ADR-0206 boundary for both aggregate dispatches (`gate` names which):
/// the brake is exactly the "why did this not go out" an operator asks about,
/// and the deferral row alone says only that something was withheld, never who
/// withheld it. The refusal rides beside the deferral rather than replacing
/// it — a release still has to know which orders it owes.
pub(super) fn gate_aggregate(
    record: &BloomRecord,
    bloom: BloomId,
    gate: &'static str,
    stage: StageId,
    dispatch: Decision,
) -> Vec<Decision> {
    EffectBoundary::new(gate, bloom, None)
        .require(
            "not_on_operator_hold",
            || record.operator_hold.is_none(),
            || {
                reads![
                    held_by: record.operator_hold.as_ref().map_or_else(String::new, |hold| hold.operator.clone()),
                    stage: format!("{stage:?}"),
                ]
            },
        )
        .effects_or(|| alloc::vec![Decision::DeferAggregate { bloom, stage }], || alloc::vec![dispatch])
}

/// Rebuild the aggregate-verify work order from the catalog, fold, and `roll` as
/// they stand — the release's half of [`aggregate_verify_dispatch`]. Never
/// consults the hold flag: the release has already decided to emit the dispatch.
///
/// `roll` is an argument for the reason [`owed_aggregate_review`]'s is: a
/// park-adopt re-arm resets the cursor in the same decision set the dispatch
/// rides in, so the record's stored count is not yet the roll the gate should
/// see.
pub(super) fn owed_aggregate_verify(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
    roll: u32,
) -> Decision {
    let binding = stage_binding(&record.stage_catalog, StageId::AggregateVerify);

    Decision::DispatchAggregateVerify {
        bloom,
        transformation: Transformation::for_aggregate_verify(&binding, tree, head, record.spec.base()),
        roll,
        profile: binding.profile,
    }
}

/// Rebuild the aggregate-review work order from the catalog, fold, and `roll`
/// as they stand — the release's half of [`aggregate_review_dispatch`]. `roll`
/// is an argument because a park-adopt re-arm resets the cursor in the same
/// decision set the dispatch rides in, so the record's stored count is not
/// yet the roll the critic should see.
pub(super) fn owed_aggregate_review(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    head: Digest,
    roll: u32,
) -> Decision {
    let binding = stage_binding(&record.stage_catalog, StageId::AggregateReview);

    Decision::DispatchAggregateReview {
        bloom,
        transformation: Transformation::for_aggregate_review(&binding, tree, head, record.spec.base()),
        roll,
        profile: binding.profile,
        configs: record.spec.configs().clone(),
    }
}

/// Reduce a whole-bloom aggregate-verify verdict — the composition workpiece's
/// `Verify` (ADR-0191 §2).
///
/// A passing verdict records the mechanical half of the composite-gate join and
/// resolves the bloom if the critic has already returned on the same fold —
/// both gates went out together, so nothing is dispatched from here either way.
///
/// A failing verdict repairs *in the composition*: the finding is filed on the
/// composition's channel and its weave repair is dispatched against the tree
/// that failed to build. No member's
/// claim is revoked and no member is dispatched — a compile failure over the
/// fold belongs to the composition, which is now a subject that can hold it
/// (ADR-0191 §4/§5). Once the stage's own catalog budget is spent the bloom
/// parks to the owner rather than re-weaving a combination that has not built
/// yet.
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
        // The gates ran over this exact tree and passed, so the verdict is
        // filed as a proof of it (#4891): a later fold that produces the same
        // tree — or a member handed it back unchanged — passes on this record
        // rather than re-running them.
        effects.extend(proof_of(*bloom, StageId::AggregateVerify, evidence));
        // The mechanical half of the composite-gate join. Filed whether or not
        // it completes the pair, so the critic's own arrival can read it. Kept
        // separate from the verify proof above because the proof is a statement
        // about a *tree* under a gate set and outlives this fold, while the
        // join is a statement about the fold now held.
        effects.push(Decision::RecordAggregateGatePass { bloom: *bloom, stage: StageId::AggregateVerify });

        if !record.aggregate_passed.contains(&StageId::AggregateReview) {
            // The critic is already judging this same fold — both gates went
            // out together — so nothing is dispatched here. The fold stays
            // held; the review's verdict is what consumes it.
            return Decisions { outcome: Outcome::AggregateVerifyPassed { bloom: *bloom, rolls }, effects };
        }

        let (resolved, resolution) = super::review::resolution_effects(record, *bloom, &integration);
        effects.extend(resolution);
        return Decisions { outcome: Outcome::Resolved(resolved), effects };
    }

    if at_park_ceiling(record, StageId::AggregateVerify, rolls) {
        // The budget is spent on a fold that still does not build. The fold
        // stays held as the owner's decision context — the same bloom-scope park
        // the review's ceiling raises, so an adopting answer that names the
        // question re-arms the cycle. The refusal files its finding first
        // (#4977): spending the budget does not make a refused fold any less a
        // refusal of the composed tree, and the channel is where a refusal's
        // evidence lives whether it goes on to re-weave or to park.
        effects.push(finding_of(*bloom, integration.tree, evidence, &[]));
        effects.push(Decision::RecordReviewPark { bloom: *bloom, question: Some(evidence.detail) });
        return Decisions {
            outcome: Outcome::AggregateVerifyParked { bloom: *bloom, rolls, question: evidence.detail },
            effects,
        };
    }

    // A compile failure over the fold implicates no member in particular — it
    // belongs to the combination — so the finding names none and the repair runs
    // at the seam.
    let repair = reweave(
        record,
        bloom,
        &Refusal {
            refused_at: StageId::AggregateVerify,
            tree: integration.tree,
            head: integration.head,
            evidence,
            implicated: &[],
        },
    );
    effects.extend(repair.effects);

    Decisions { outcome: repair.outcome, effects }
}
