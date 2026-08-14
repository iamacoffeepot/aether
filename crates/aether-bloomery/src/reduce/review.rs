//! The composition workpiece's `Review` (ADR-0191 §3): one verdict over the
//! composed head, bounded by the two-pass ceiling that parks the bloom to the
//! owner rather than buying a third roll (ADR-0153).
//!
//! Its subject is the weave — the reconcile-authored seam edits, the files more
//! than one member touched, and whether each member's work order is still
//! visibly satisfied in the composed tree. The member work orders and candidates
//! are reference input; the member diffs are not re-read. So a refusal repairs
//! the weave and never re-opens a member.

use alloc::vec::Vec;

use super::attempt::stage_binding;
use super::composition::{Refusal, reweave};
use super::{
    AggregateReviewError, AggregateReviewFault, BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration,
    Outcome, Snapshot,
};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{CompositionFinding, Evidence, EvidenceKind, ResolvedBloom, Transformation};

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

/// Reduce a composition-review verdict (ADR-0153, ADR-0191 §3). A passing
/// verdict resolves the bloom from its held fold — [`Decision::SetResolved`]
/// plus the [`Decision::DispatchLand`] the land reactor consumes. A failing
/// verdict files the finding on the composition's channel and dispatches the
/// weave repair against the composed tree; the fold stays held, because it is
/// the composition's candidate under repair rather than someone else's stale
/// artifact. The second failing verdict parks the bloom to the owner — the
/// two-pass ceiling; the machine never buys a third roll, though an adopting
/// answer lets the owner buy a fresh cycle.
///
/// Which of the two a verdict is, is now the *reviewer's* statement about its
/// findings rather than the mere existence of one (#4961). A review whose
/// findings are all judgment advisories reports as a pass and arrives here as
/// one, carrying [`EvidenceKind::ReviewAdvisory`] — so the observations are
/// filed on the composition's channel on the way to the landing, and a
/// subjective finding costs a bloom nothing.
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
    // The implication is a *label on the finding*, not a routing table: it names
    // which members' intent the verdict thinks the weave lost, so the recorded
    // finding points a reader (and any follow-up work filed from it) at the right
    // code. A named non-member is malformed and is validated before any effect
    // applies, so such a verdict changes nothing — checked ahead of the verdict
    // split because a passing verdict can carry an advisory finding now, and a
    // label the record cannot resolve is no better on that row than on a refusal's.
    // An empty implication is never expanded to every member: under ADR-0191 there
    // is nothing to over-route *to*, and a verdict about the weave as a whole is
    // exactly a finding that names nobody.
    if let Some(stranger) =
        implicated.iter().find(|wp| !record.spec.members().iter().any(|member| member.workpiece == **wp))
    {
        return Decisions::rejected(Outcome::AggregateReviewRejected(AggregateReviewError::NotAMember(
            stranger.clone(),
        )));
    }
    let rolls = record.aggregate_rolls + 1;
    let mut effects = alloc::vec![
        Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() },
        Decision::RecordAggregateRoll { bloom: *bloom, rolls },
    ];
    if passed {
        // A pass that still recorded judgment findings (#4961). The reviewer
        // marked none of them blocking, so nothing here re-weaves, spends the
        // repair budget, or delays the landing — and the observations still land
        // on the composition's own channel, where an operator can adjudicate
        // them and the study that files fix-forward work can read them. Filed
        // before the resolution effects so the journal shows the finding under
        // the verdict that raised it.
        if evidence.kind == EvidenceKind::ReviewAdvisory {
            effects.push(Decision::RecordCompositionFinding {
                bloom: *bloom,
                finding: CompositionFinding {
                    subject: integration.tree,
                    detail: evidence.detail,
                    implicated: implicated.to_vec(),
                },
            });
        }
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
    // First blocking verdict: repair in the composition (ADR-0191 §4/§5). The
    // implicated set is recorded on the finding — it files follow-up work for a
    // member whose code the verdict genuinely names, and it directs the reader —
    // but nothing is dispatched against a member and no claim is revoked. A
    // member that passed its review is done; the weave is what repairs, at the
    // seam, against the composed tree that was refused.
    let repair = reweave(
        record,
        bloom,
        &Refusal {
            refused_at: StageId::AggregateReview,
            tree: integration.tree,
            head: integration.head,
            evidence,
            implicated,
        },
    );
    effects.extend(repair.effects);

    Decisions { outcome: repair.outcome, effects }
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
