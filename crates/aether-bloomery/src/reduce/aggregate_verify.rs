//! The whole-bloom aggregate verify: the mechanical gate over the folded head,
//! run beside the critic over the same fold.
//!
//! Every member verified its own candidate in isolation and passed. The fold is
//! the first tree that carries all of them at once, so it is the first thing
//! that can fail on their interaction — two members that each compile and
//! together do not. Without this gate the landing CI is what discovers that,
//! downstream of the point where the bloom can still route it back to an owner.

use alloc::borrow::ToOwned as _;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::attempt::stage_binding;
use super::boundary::EffectBoundary;
use super::composition::{Refusal, finding_of, reweave};
use super::gate::{AGGREGATE_REVIEW_GATE, AGGREGATE_VERIFY_GATE};
use super::verify_memo::proof_of;
use super::{
    AggregateVerifyError, BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration, Outcome, Snapshot,
};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId};
use crate::reads;
use crate::values::{Evidence, OperatorHold, RedVerify, Transformation};

/// Fallback when a sealed catalog binds no retry budget: one attempt, then the ceiling.
pub(super) const DEFAULT_RETRY_BUDGET: u32 = 1;

/// Who a reducer-authored operator hold records as the decider — the same
/// answer a reducer-authored ejection records, and for the same reason: the
/// coordinator acted on the disposition a bloom sealed.
const HOLDING_DECIDER: &str = "bloomery";

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
    let (record, integration) = match judged_fold(snapshot, bloom, evidence) {
        Ok(judged) => judged,
        Err(error) => return Decisions::rejected(Outcome::AggregateVerifyRejected(error)),
    };

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
        effects.extend(proof_of(record, *bloom, StageId::AggregateVerify, evidence));
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

        let (resolved, resolution) = super::review::resolution_effects(record, *bloom, integration);
        effects.extend(resolution);
        return Decisions { outcome: Outcome::Resolved(resolved), effects };
    }

    // Low tolerance (ADR-0218 §Amendment): a red fold has no single member to
    // eject, so the bloom stops here rather than buying a re-weave. Parked on
    // the first red instead of at the catalog ceiling below, and parked *under
    // an operator hold* rather than only on the question channel: the hold is
    // what stops every other dispatch this bloom would otherwise go on making
    // while nobody is looking at the fold that did not build.
    if record.red_verify == RedVerify::Eject {
        return parked_on_the_fold(
            record,
            *bloom,
            integration.tree,
            evidence,
            rolls,
            effects,
            &format!(
                "the aggregate verify over fold {} came back red and the bloom's sealed disposition does not \
                 re-weave; read its findings under evidence {} and release the hold once the fold is answered",
                integration.tree.to_hex(),
                evidence.detail.to_hex()
            ),
        );
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

/// The sealed bloom and the fold a verdict is entitled to act on, or the typed
/// [`AggregateVerifyError`] that says why it is not.
///
/// The three guards every whole-bloom mechanical verdict passes, named once
/// because two facts now reach them — [`Fact::AggregateVerifyCompleted`] and
/// the documentation refusal below — and a second copy would let one of them
/// drift into accepting a stale verdict against a newer integration.
///
/// [`Fact::AggregateVerifyCompleted`]: crate::Fact::AggregateVerifyCompleted
fn judged_fold<'a>(
    snapshot: &'a Snapshot,
    bloom: &BloomId,
    evidence: &Evidence,
) -> Result<(&'a BloomRecord, &'a FoldedIntegration), AggregateVerifyError> {
    let Some(record) = snapshot.blooms.get(bloom).filter(|record| record.status == BloomStatus::Sealed) else {
        return Err(AggregateVerifyError::UnknownOrInactiveBloom);
    };
    let Some(integration) = record.integration.as_ref() else {
        return Err(AggregateVerifyError::NoPendingIntegration);
    };
    // The verdict must bind the exact tree the held fold produced — a stale
    // verdict from a superseded fold cannot act on a newer integration.
    if !evidence.validates(&integration.tree) {
        return Err(AggregateVerifyError::SubjectMismatch { expected: integration.tree, got: evidence.subject });
    }

    Ok((record, integration))
}

/// How many documentation-repair laps one bloom's product may buy before the
/// question goes to an operator.
///
/// Two, and its own number rather than the sealed catalog's `AggregateVerify`
/// budget that [`at_park_ceiling`] reads. That budget governs how many times a
/// *fold* may be re-woven, and ADR-0218's low tolerance deliberately spends
/// none of it: a compile red parks on the first verdict. This ceiling is the
/// opposite calibration for the opposite defect — a rustdoc diagnostic names
/// the file and the line, so the first lap almost always answers it, and the
/// second exists for the case where fixing one link exposed another. A third
/// would be the machine guessing, which is the thing the amendment refuses.
///
/// Counted from [`BloomRecord::aggregate_verify_rolls`] rather than a counter
/// of its own, because that is the honest number: every documentation lap ends
/// by re-dispatching the aggregate gates over the repaired weave, so each lap
/// costs exactly one roll, and a bloom that reached this gate three times has
/// had two chances to fix its documentation whatever else happened in between.
/// A field would be a second thing to keep true.
const DOCS_REPAIR_LAPS: u32 = 2;

/// Reduce a final aggregate verify that came back red on documentation and
/// nothing else (ADR-0218 §Amendment: documentation is judged once, over the
/// product).
///
/// The product is assembled, every member has passed, and the one thing
/// standing between the bloom and its landing is a diagnostic that already
/// names a file and a line. So this refusal does not eject a member and does
/// not park on the first red: it opens a repair lap on the composition — the
/// ADR-0191 weave repair, whose work order is the refusing evidence and whose
/// subject is the woven tree — and the lap's own completion re-dispatches the
/// aggregate gates, which is how the documentation pass gets re-run. Nothing
/// here duplicates that machinery.
///
/// [`BloomRecord::red_verify`] is deliberately not consulted. It chooses
/// between ejecting a member and repairing one, and this refusal implicates no
/// member at all: a documentation defect in the product is the composition's,
/// and the composition's answer to a refusal is the same lap under either
/// disposition.
///
/// At [`DOCS_REPAIR_LAPS`] the bloom parks with the findings, the same way a
/// red fold parks — a person reading a diagnostic two laps failed to clear is
/// faster than a third lap.
pub(super) fn reduce_aggregate_docs_refused(snapshot: &Snapshot, bloom: &BloomId, evidence: &Evidence) -> Decisions {
    let (record, integration) = match judged_fold(snapshot, bloom, evidence) {
        Ok(judged) => judged,
        Err(error) => return Decisions::rejected(Outcome::AggregateVerifyRejected(error)),
    };

    let rolls = record.aggregate_verify_rolls + 1;
    let mut effects = alloc::vec![
        Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() },
        Decision::RecordAggregateVerifyRoll { bloom: *bloom, rolls },
    ];

    if rolls > DOCS_REPAIR_LAPS {
        return parked_on_the_fold(
            record,
            *bloom,
            integration.tree,
            evidence,
            rolls,
            effects,
            &format!(
                "the aggregate verify over product {} came back red on documentation for the {DOCS_REPAIR_LAPS}th \
                 time; read its findings under evidence {} and release the hold once the documentation is answered",
                integration.tree.to_hex(),
                evidence.detail.to_hex()
            ),
        );
    }

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

/// Park a bloom whose fold did not build, under an operator hold naming the
/// aggregate and what the gate said (ADR-0218 §Amendment: low tolerance).
///
/// The member-side rule of the amendment ejects, and this is its whole-bloom
/// counterpart: there is no member to eject, because a fold that does not
/// build is a statement about the combination rather than about any one
/// contribution. So the bloom stops instead, with three things recorded
/// together — the finding on the composition's channel, the question that
/// holds the fold as the owner's decision context, and the hold that makes
/// "why did nothing else go out" answerable without reading the journal.
///
/// Nothing is dispatched. That is the point: the ADR-0191 re-weave is a paid
/// lap on a combination no verdict has yet accepted, and the amendment's whole
/// claim is that a person reading the findings is the faster path.
///
/// `reason` is the hold's own sentence, because the two refusals that reach
/// here stop for different causes and an operator reading the brake needs which
/// one: a fold that does not build, or documentation two repair laps failed to
/// clear. Everything else about the park is identical, so it is one function
/// with the sentence handed in rather than two that drift.
fn parked_on_the_fold(
    record: &BloomRecord,
    bloom: BloomId,
    tree: Digest,
    evidence: &Evidence,
    rolls: u32,
    mut effects: Vec<Decision>,
    reason: &str,
) -> Decisions {
    effects.push(finding_of(bloom, tree, evidence, &[]));
    effects.push(Decision::RecordReviewPark { bloom, question: Some(evidence.detail) });
    // An already-held bloom keeps the hold it has: the operator's own words
    // outrank a machine-authored sentence, and `reduce_operator_hold` refuses a
    // second hold for the same reason.
    if record.operator_hold.is_none() {
        effects.push(Decision::RecordOperatorHold {
            bloom,
            hold: OperatorHold { reason: String::from(reason), operator: HOLDING_DECIDER.to_owned() },
        });
    }

    Decisions { outcome: Outcome::AggregateVerifyParked { bloom, rolls, question: evidence.detail }, effects }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use crate::ids::{BloomId, StageId, WorkpieceId};
    use crate::reduce::{Decision, Decisions, Fact, Outcome, Snapshot};
    use crate::testing::{claim, digest, draft, event, membership, step};
    use crate::values::{Evidence, EvidenceKind};

    /// Two members folded into a held integration — the position a whole-bloom
    /// mechanical verdict arrives at.
    fn folded_bloom() -> (Snapshot, BloomId) {
        let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
        let bloom = spec.id();
        let snapshot = Snapshot::new(digest(1)).with_green_base(digest(1));
        let (snapshot, _) = step(&snapshot, &event("seal", Fact::Seal(spec)));
        let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
        let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
        let (snapshot, _) = step(
            &snapshot,
            &event("fold", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }),
        );

        (snapshot, bloom)
    }

    fn docs_refused(bloom: BloomId) -> Fact {
        Fact::AggregateDocsRefused {
            bloom,
            evidence: Evidence { subject: digest(40), kind: EvidenceKind::VerificationResult, detail: digest(52) },
        }
    }

    fn held(decisions: &Decisions) -> bool {
        decisions.effects.iter().any(|effect| matches!(effect, Decision::RecordOperatorHold { .. }))
    }

    fn repairs_the_composition(decisions: &Decisions) -> bool {
        decisions.effects.iter().any(|effect| {
            matches!(
                effect,
                Decision::AdvanceStage { workpiece, progress, .. }
                    if *workpiece == WorkpieceId::composition() && progress.stage == StageId::Refine
            )
        })
    }

    #[test]
    fn a_documentation_refusal_opens_a_repair_lap_instead_of_parking_the_bloom() {
        // The plausible bug: the documentation refusal falls through to the
        // low-tolerance park that a red fold takes, and a broken intra-doc link
        // — a diagnostic naming a file and a line — stops the bloom dead and
        // waits for a person, which is the placement this amendment exists to
        // undo.
        let (snapshot, bloom) = folded_bloom();

        let decisions = super::reduce_aggregate_docs_refused(&snapshot, &bloom, &evidence_of(&docs_refused(bloom)));

        assert!(repairs_the_composition(&decisions), "the product repairs its own documentation: {decisions:?}");
        assert!(!held(&decisions), "a documentation refusal does not brake the bloom: {decisions:?}");
        assert!(
            !decisions.effects.iter().any(|effect| matches!(effect, Decision::RecordWithdrawal { .. })),
            "and it ejects nobody: {decisions:?}",
        );
    }

    #[test]
    fn the_lap_past_the_documentation_ceiling_parks_with_its_findings() {
        // The plausible bug: the ceiling is never reached, so a product whose
        // documentation two laps could not clear re-weaves forever on a
        // diagnostic the model is not converging on.
        let (snapshot, bloom) = folded_bloom();
        let refusal = docs_refused(bloom);
        let mut snapshot = snapshot;
        for lap in 0..super::DOCS_REPAIR_LAPS {
            let stepped = step(&snapshot, &event(&alloc::format!("docs-{lap}"), refusal.clone()));
            snapshot = stepped.0;
        }

        let decisions = super::reduce_aggregate_docs_refused(&snapshot, &bloom, &evidence_of(&refusal));

        assert!(
            matches!(decisions.outcome, Outcome::AggregateVerifyParked { .. }),
            "the ceiling parks: {:?}",
            decisions.outcome,
        );
        assert!(
            decisions.effects.iter().any(|effect| matches!(effect, Decision::RecordCompositionFinding { .. })),
            "the operator gets the findings that stopped it: {decisions:?}",
        );
        assert!(
            decisions.effects.iter().any(|effect| matches!(
                effect,
                Decision::RecordOperatorHold { hold, .. } if hold.reason.contains("documentation")
            )),
            "and a brake that says documentation stopped it: {decisions:?}",
        );
    }

    fn evidence_of(fact: &Fact) -> Evidence {
        match fact {
            Fact::AggregateDocsRefused { evidence, .. } => evidence.clone(),
            other => panic!("not a documentation refusal: {other:?}"),
        }
    }
}
