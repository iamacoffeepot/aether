//! The manager override (#4957): the two moves that belong to an operator
//! rather than to a verdict.
//!
//! A bloom stops in one of two shapes the machine cannot answer. Its
//! composition exhausted a gate's budget and parked under a finding nobody is
//! going to repair by re-rolling — the remaining defect is one a person has read
//! and is prepared to answer for. Or a workpiece wedged with a fix that is
//! obvious to write and expensive to buy another model lap for. Before this the
//! only levers were another lap, a supersession, or abandonment; two live
//! incidents on 2026-08-14 fell into exactly that gap.
//!
//! Both moves are journal-first: the REST edge appends the fact and nothing
//! else, and every state movement is decided here. And both stay inside what an
//! override may decide.
//!
//! - **Adjudication closes findings.** It never reopens or dispatches a member —
//!   members are immutable after review (ADR-0191 §4) — and it never invents a
//!   verdict: what it closes has to be a finding the composition's own gates
//!   raised.
//! - **Repair supplies a candidate.** It re-enters at `Verify`, so the
//!   mechanical suite and the delta-confirm review still run over it. Only the
//!   model lap is skipped, which makes it a choice about who writes the code and
//!   never about who judges it.
//!
//! Neither substitutes for an approval (ADR-0181). Both doors refuse a bloom
//! whose membership is not fully approved rather than carrying it toward a
//! landing: an override adjudicates findings and retry budgets, and a member
//! whose sealed approval resolves above `auto` keeps needing its signed
//! statement no matter what reason string accompanies the request.

use alloc::vec::Vec;

use super::aggregate_verify::aggregate_verify_dispatch;
use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate};
use super::composition::composition_progress;
use super::{
    AdjudicationError, BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration, OperatorRepairError, Outcome,
    Snapshot, StageProgress,
};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{
    Adjudication, CandidateRef, Disposition, EvidenceKind, OperatorRepair, ResolvedBloom, VerifyFailureSet,
};

/// The ADR-0181 line, checked at both doors: every member's sealed approval
/// binds that member's own subject as an [`EvidenceKind::Approval`].
///
/// The seal door already refuses a membership that fails this
/// ([`SealError::UnapprovedMember`](crate::SealError::UnapprovedMember)), which
/// is exactly why the override doors re-check it rather than assuming it. An
/// override is the one act whose authority is an unsigned operator identity, so
/// it is the one place where a record that reached this state by any other route
/// — a hand-repaired store, a future admission door, a reducer bug — would
/// convert "I read the findings" into "the approval was not needed". The
/// re-check makes that unrepresentable: an override may spend retry budgets and
/// close findings, and a member above `auto` still needs its signed statement.
fn unapproved_member(record: &BloomRecord) -> Option<&WorkpieceId> {
    record
        .spec
        .members()
        .iter()
        .find(|member| member.approval.kind != EvidenceKind::Approval || !member.approval.validates(&member.subject()))
        .map(|member| &member.workpiece)
}

/// Whether an operator-supplied string says anything at all. A blank reason or
/// operator is refused rather than defaulted at both doors — the audit trail is
/// the override's whole product.
fn stated(text: &str) -> bool {
    !text.trim().is_empty()
}

/// Whether `finding` names something this bloom actually raised and has not
/// already closed — the line between adjudicating a finding and inventing one.
///
/// The general path is the findings channel: every refusal of the composed tree
/// files a [`CompositionFinding`](crate::CompositionFinding) on it, the three
/// gate ceilings that park included (#4977), so the parked bloom this override
/// most exists for is adjudicated the same way a re-weaving refusal's finding
/// is.
///
/// The park marker is the second, narrow arm, for the two parks the channel does
/// not carry. **Legacy journals**: a park written before #4977 filed no finding,
/// and boot replay folds the decisions as they were recorded (ADR-0190), so the
/// only artifact such a record holds is the marker. And the **admitted-question
/// park** ([`super::evidence::reduce_admit_evidence`]), which raises the
/// bloom-scope park from a reviewer's `Question` rather than from a gate
/// refusing the tree, and files nothing on the channel. Either way the marker
/// holds the verdict artifact a filed finding would carry as its `detail`, which
/// is exactly what an operator reading a parked bloom is reading.
fn adjudicable(record: &BloomRecord, finding: &Digest) -> bool {
    record.open_composition_findings().any(|open| open.detail == *finding) || record.review_park == Some(*finding)
}

/// Reduce an operator adjudication ([`Fact::OperatorAdjudication`](crate::Fact::OperatorAdjudication)).
///
/// The named findings are closed and any park they raised is released. Landing
/// is a separate question: the composition proceeds from the weave it already
/// holds only when that weave's aggregate verify is already green — the same
/// destination a passing review sends it to, reached on the operator's
/// authority instead of a verdict's. A red or still-pending proof closes the
/// findings and leaves the ordinary refine cycle to finish proving the head;
/// landing dispatches once that proof is green, not from this override.
///
/// What it never emits is a member dispatch or a
/// [`Decision::RevokeResolution`]. An
/// adjudication is a statement about the composition's findings, and a member
/// that has passed its review is done (ADR-0191 §4).
pub(super) fn reduce_operator_adjudication(
    snapshot: &Snapshot,
    bloom: &BloomId,
    adjudication: &Adjudication,
) -> Decisions {
    let rejected = |error: AdjudicationError| Decisions::rejected(Outcome::AdjudicationRejected(error));

    // A `Sealed` bloom is mid-line and a `Resolved` one is awaiting its landing;
    // both can hold open findings. A landed or superseded bloom is past any of
    // this, so there is nothing for an adjudication to act on.
    let Some(record) = snapshot
        .blooms
        .get(bloom)
        .filter(|record| matches!(record.status, BloomStatus::Sealed | BloomStatus::Resolved))
    else {
        return rejected(AdjudicationError::UnknownOrInactiveBloom);
    };
    if !stated(&adjudication.reason) {
        return rejected(AdjudicationError::BlankReason);
    }
    if !stated(&adjudication.operator) {
        return rejected(AdjudicationError::BlankOperator);
    }
    if adjudication.disposition == (Disposition::Deferred { issue: 0 }) {
        return rejected(AdjudicationError::DeferredWithoutIssue);
    }
    if let Some(workpiece) = unapproved_member(record) {
        return rejected(AdjudicationError::UnapprovedMember(workpiece.clone()));
    }
    if adjudication.findings.is_empty() {
        return rejected(AdjudicationError::NoFindings);
    }
    if let Some(stranger) = adjudication.findings.iter().find(|finding| !adjudicable(record, finding)) {
        return rejected(AdjudicationError::UnknownFinding(*stranger));
    }

    let mut effects = alloc::vec![Decision::RecordAdjudication { bloom: *bloom, adjudication: adjudication.clone() }];
    // A park raised by one of the closed findings is released here rather than
    // by an adopting answer: the adjudication *is* the answer, in the one form
    // that carries a reason the landing can quote. A park under some other
    // question stands — closing a finding says nothing about it.
    if let Some(question) = record.review_park.filter(|question| adjudication.findings.contains(question)) {
        effects.push(Decision::ReleaseHold { bloom: *bloom, question });
        effects.push(Decision::RecordReviewPark { bloom: *bloom, question: None });
    }
    let proceeds = proceed_to_landing(record, bloom, &mut effects);

    Decisions {
        outcome: Outcome::FindingsAdjudicated {
            bloom: *bloom,
            closed: adjudication.findings.clone(),
            proceeds_to_landing: proceeds,
        },
        effects,
    }
}

/// Send the adjudicated composition to its landing, if it has a proven weave.
///
/// Three states, one destination. A bloom already `Resolved` was parked by its
/// landing gate, so it re-proposes the head it still holds. A `Sealed` bloom
/// holding a fold has passed its members and stopped at a composite gate, so it
/// resolves from that fold exactly as a passing review would have resolved it. A
/// `Sealed` bloom holding neither is a composition whose landing refusal cleared
/// the fold and whose repair then wedged — there is no tree to land, so the
/// findings close and nothing dispatches; the operator's next move there is a
/// repair, not a waiver.
///
/// A fourth refusal sits in front of every destination: the head about to be
/// proposed must already carry a green aggregate-verify proof (#5104). A refine
/// lap can replace the held fold while a park still names an earlier, proven
/// weave, and landing that newer head before its gates return — or after they
/// return red — is how a waived finding became a red landing proposal. Closing
/// the findings does not wait on that proof; only this dispatch does.
///
/// Either landing path first advances the composition's cursor to `Land`, which
/// is what clears a composition wedge (`AdvanceStage` is the only route out of
/// the wedged set) and what makes the projection say the weave moved rather than
/// leaving a resolved bloom reporting a stopped composition.
fn proceed_to_landing(record: &BloomRecord, bloom: &BloomId, effects: &mut Vec<Decision>) -> bool {
    let composition = WorkpieceId::composition();
    if record.status == BloomStatus::Resolved {
        let Some(head) = record.resolved_head else {
            return false;
        };
        if !resolved_head_is_proven(record, head) {
            return false;
        }
        effects.push(Decision::AdvanceStage {
            bloom: *bloom,
            workpiece: composition,
            // The landing gate binds the head it judged and the bloom no longer
            // holds the fold that produced it, so the head stands in for both
            // digests — the same substitution `reduce_landing_rejected` makes.
            progress: composition_progress(StageId::Land, 1, CandidateRef { tree: head, checkout: head }),
        });
        effects.push(Decision::DispatchLand { bloom: *bloom, expected_base: record.spec.base(), new_head: head });
        return true;
    }
    let Some(integration) = record.integration.as_ref() else {
        return false;
    };
    if !weave_is_proven(record, integration.tree) {
        return false;
    }
    let weave = CandidateRef { tree: integration.tree, checkout: integration.head };
    effects.push(Decision::AdvanceStage {
        bloom: *bloom,
        workpiece: composition,
        progress: composition_progress(StageId::Land, 1, weave),
    });
    // The fold is consumed on resolve, exactly as a passing review consumes it:
    // a resolved bloom holds no pending gate run.
    effects.push(Decision::RecordIntegration { bloom: *bloom, integration: None });
    effects.push(Decision::SetResolved {
        bloom: *bloom,
        resolved: ResolvedBloom {
            bloom: *bloom,
            tree: integration.tree,
            head: integration.head,
            lineage: integration.lineage.clone(),
            resolution_claims: record.claims.values().cloned().collect(),
        },
    });
    effects.push(Decision::DispatchLand {
        bloom: *bloom,
        expected_base: record.spec.base(),
        new_head: integration.head,
    });
    true
}

/// Whether `tree` has a green mechanical proof on this bloom — the memo every
/// other verify position consults (#4891). A missing proof is the failed-or-pending
/// case: either the gates have not returned or they returned red, and neither is
/// a head landing may propose.
fn weave_is_proven(record: &BloomRecord, tree: Digest) -> bool {
    record.verify_proof_for(tree).is_some()
}

/// Whether a `Resolved` bloom's landing head is a weave this bloom has already
/// proven. The fold is cleared on resolve, so the composition cursor's candidate
/// is what still names the tree that produced `head`. A resolved bloom with no
/// matching cursor was reviewed onto this head and refine cannot replace
/// `resolved_head` while the bloom stays `Resolved`, so that case stays landable.
fn resolved_head_is_proven(record: &BloomRecord, head: Digest) -> bool {
    match record.progress.get(&WorkpieceId::composition()).and_then(|progress| progress.candidate) {
        Some(candidate) if candidate.checkout == head => weave_is_proven(record, candidate.tree),
        _ => true,
    }
}

/// Reduce an operator-supplied repair ([`Fact::OperatorRepair`](crate::Fact::OperatorRepair)).
///
/// The candidate re-enters the workpiece's line at `Verify`. For a member that
/// is the ordinary
/// [`Decision::DispatchAttempt`] pair every
/// other cursor move emits — a `Verify` dispatch, not a claim — so the
/// mechanical suite runs, a failure routes through
/// [`Fact::VerifyFailed`](crate::Fact::VerifyFailed) and charges the repair roll
/// it always charged, and a pass integrates through the same door. For the
/// composition it is the composite gate run over the operator's weave, which is
/// the same position a returning weave repair lands on.
///
/// The cursor's spent counters ride across unchanged. An operator writing the
/// candidate buys the workpiece a lap, never a fresh budget: the gates stay
/// honest, which is the entire difference between this and a waiver.
pub(super) fn reduce_operator_repair(snapshot: &Snapshot, bloom: &BloomId, repair: &OperatorRepair) -> Decisions {
    let rejected = |error: OperatorRepairError| Decisions::rejected(Outcome::OperatorRepairRejected(error));

    let Some(record) = snapshot.blooms.get(bloom).filter(|record| record.status == BloomStatus::Sealed) else {
        return rejected(OperatorRepairError::UnknownOrInactiveBloom);
    };
    if !stated(&repair.reason) {
        return rejected(OperatorRepairError::BlankReason);
    }
    if !stated(&repair.operator) {
        return rejected(OperatorRepairError::BlankOperator);
    }
    if let Some(workpiece) = unapproved_member(record) {
        return rejected(OperatorRepairError::UnapprovedMember(workpiece.clone()));
    }
    // Held means held (#4976). A repair's entire product is the `Verify`
    // dispatch it emits, and a held bloom emits none — so admitting one would
    // record an operator's candidate, dispatch nothing, and answer them as
    // though the gates were running over it. The order is the operator's to
    // choose and it is not ambiguous: release the bloom, then repair it.
    //
    // The line this draws is not "every door refuses while held". A door refuses
    // here when the dispatch is the whole of what it produces; a door whose
    // product the hold does not gate proceeds, with any dispatch it emits
    // deferred by the ordinary choke. A grant is a budget move and takes the
    // deferral; an adjudication closes findings and dispatches a landing, which
    // a hold never gated — and refusing it would freeze the one act a bloom is
    // usually held *in order* to make.
    if record.operator_hold.is_some() {
        return rejected(OperatorRepairError::Held);
    }
    // Only a stopped workpiece is repairable, for the reason only a wedged
    // member is grantable: one still holding a dispatched attempt would end up
    // with two workers on it.
    if !record.wedged.contains_key(&repair.workpiece) {
        return rejected(OperatorRepairError::NotWedged(repair.workpiece.clone()));
    }

    let recorded = Decision::RecordOperatorRepair { bloom: *bloom, repair: repair.clone() };
    if repair.workpiece.is_composition() {
        return rewoven_by_operator(record, bloom, repair, recorded);
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == repair.workpiece) else {
        return rejected(OperatorRepairError::NotAMember(repair.workpiece.clone()));
    };
    // A member carrying a resolution claim has passed its review and is
    // immutable (ADR-0191 §4). Unreachable through the wedge check above under
    // today's transitions — a resolved member is not wedged — and stated anyway,
    // because it is the rule this door must not become a way around.
    if record.claims.contains_key(&repair.workpiece) {
        return rejected(OperatorRepairError::AlreadyResolved(repair.workpiece.clone()));
    }
    let cursor = record.progress.get(&repair.workpiece).copied();
    let progress = StageProgress {
        stage: StageId::Verify,
        attempts: 1,
        candidate: Some(repair.candidate),
        // Carried, not reset: the repair rolls and the failure identities this
        // member has already spent are what keep a bad operator fix bouncing on
        // the same terms a bad lane's does.
        repair_rolls: cursor.map_or(0, |cursor| cursor.repair_rolls),
        seen_verify_failures: cursor.map_or(VerifyFailureSet::EMPTY, |cursor| cursor.seen_verify_failures),
        fold_checkpoint: cursor.and_then(|cursor| cursor.fold_checkpoint),
        fold_conflict_evidence: None,
    };
    let mut effects = alloc::vec![recorded];
    effects.extend(move_effects_with_candidate(
        *bloom,
        &repair.workpiece,
        member.scope_revision,
        progress,
        DispatchTargets { subject: repair.candidate.tree, checkout: repair.candidate.checkout },
        Some(repair.candidate.tree),
        SealedLine::of(record, member),
    ));

    Decisions {
        outcome: Outcome::OperatorRepairAccepted {
            bloom: *bloom,
            workpiece: repair.workpiece.clone(),
            candidate: repair.candidate.tree,
        },
        effects,
    }
}

/// An operator-supplied *weave*: the composition's own repair, arriving from a
/// person instead of from its refine loop.
///
/// It lands exactly where a returning weave repair lands (ADR-0191 §5) — the
/// operator's tree becomes the held integration, the composition's cursor
/// advances to its `Verify`, and the composite gate run dispatches over it. The
/// lineage the previous fold recorded rides along, because a repair edits the
/// composed tree rather than re-ordering what went into it.
fn rewoven_by_operator(
    record: &BloomRecord,
    bloom: &BloomId,
    repair: &OperatorRepair,
    recorded: Decision,
) -> Decisions {
    let weave = repair.candidate;

    Decisions {
        outcome: Outcome::OperatorRepairAccepted {
            bloom: *bloom,
            workpiece: repair.workpiece.clone(),
            candidate: weave.tree,
        },
        effects: alloc::vec![
            recorded,
            Decision::RecordIntegration {
                bloom: *bloom,
                integration: Some(FoldedIntegration {
                    tree: weave.tree,
                    head: weave.checkout,
                    lineage: record.integration.as_ref().map_or_else(Vec::new, |held| held.lineage.clone()),
                }),
            },
            Decision::AdvanceStage {
                bloom: *bloom,
                workpiece: WorkpieceId::composition(),
                progress: composition_progress(StageId::Verify, 1, weave),
            },
            aggregate_verify_dispatch(record, *bloom, weave.tree, weave.checkout),
        ],
    }
}
