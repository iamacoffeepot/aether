//! Admin mode (ADR-0219): the window in which a bloom is out of the machine's
//! hands and in an operator's.
//!
//! Every other operator door is a single decision the reactor acts on at once.
//! That is the right shape for a bloom that has stopped, and the wrong shape
//! for a bloom that is *running wrong*: repairing one takes several moves that
//! only make sense together, and making them one at a time against a live
//! reactor is how bloom `0f16e207` answered a red review — whose findings named
//! three members the operator had already withdrawn — by buying a model lap to
//! re-author those members' work.
//!
//! So admin mode is a session. Its shape:
//!
//! - **It dispatches nothing.** Entering records an ordinary
//!   [`OperatorHold`] beside the flag, so the one dispatch choke #4976 built is
//!   the one that holds here too — there is no second brake to forget. Exiting
//!   releases it and re-derives what is due from the cursors as they stand,
//!   through [`super::operator_hold`]'s own helpers.
//! - **It costs nothing.** While it is open an executor fault records its
//!   evidence and stops: no machinery roll, no wedge, no repair roll. A lane an
//!   operator cancels is cancelled, not failed — no fact is admitted for it at
//!   all. That is what makes reading a broken bloom free.
//! - **It moves what the doors say it moves, and nothing else.** A candidate is
//!   placed at a gate position; a re-run is a real dispatch; a dropped lap
//!   reverts one candidate. None of them touches a spent counter, because an
//!   operator supplying work buys a lap and never a fresh budget — the same
//!   line [`super::operator`] draws.
//! - **A waiver is an adjudication.** It closes findings this bloom actually
//!   raised, recorded as an [`Adjudication`] over named evidence, and records
//!   the gate's pass beside it. What it never does is mint a verdict: there is
//!   no synthesized green anywhere in this module, which is why waiving a
//!   *mechanical* gate is refused unless the request says the operator knows it
//!   is landing unproven code.
//!
//! The one thing exit does that no release does is the landing. A waived gate
//! leaves a fold whose join is complete and whose completing verdict is never
//! going to arrive, so nothing would resolve it — the bloom would sit green and
//! stationary. Exit finishes that join, and records that the landing stands on
//! a waiver.

use alloc::vec::Vec;

use super::aggregate_verify::{owed_aggregate_review, owed_aggregate_verify};
use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate, reconcile_or_line_targets};
use super::composition::composition_progress;
use super::operator::unapproved_member;
use super::operator_hold::{owed_aggregates, owed_dispatch};
use super::review::resolution_effects;
use super::{
    AdminError, BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration, Outcome, Snapshot, StageProgress,
};
use crate::digest::{Digest, digest_of};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{
    Adjudication, AdminAct, AdminActKind, AdminCandidate, AdminLaneCancel, AdminLapDrop, AdminNote, AdminRerun,
    AdminWaiver, CandidateRef, Disposition, Evidence, OperatorHold,
};

/// The stages an admin re-run may aim a workpiece from.
///
/// Stated rather than derived from the catalog because the question is not
/// "does this bloom bind that stage" but "can the record honestly run this
/// workpiece from there": a Construct or Refine re-run would re-author work the
/// operator is in the middle of replacing by hand, and a Land re-run is the
/// landing door rather than a gate. The member half and the composition half
/// are disjoint, which is what refuses a member aimed at an aggregate gate.
const MEMBER_RERUNNABLE: &[StageId] = &[StageId::Verify, StageId::Review, StageId::Reconcile];

/// The composition half of [`MEMBER_RERUNNABLE`] — the two composite gates.
const COMPOSITION_RERUNNABLE: &[StageId] = &[StageId::AggregateVerify, StageId::AggregateReview];

/// The gates whose waiver lands code no mechanical gate proved, and which
/// therefore need the operator's explicit acknowledgement.
///
/// A review waiver is a person overruling a *judgment*, which is exactly what
/// an operator is for. A verify waiver is a person overruling a *fact*, and
/// there is no reading of the record that makes that safe — so the request has
/// to say so in as many words.
const UNVERIFIED_WAIVER_GATES: &[StageId] = &[StageId::Verify, StageId::AggregateVerify];

/// The bloom an admin door may act on: known, and still running a line worth
/// repairing.
///
/// `Resolved` is included for the reason the brake includes it — a bloom
/// awaiting its landing can still be sent back into the line by a refused
/// landing, and that is a state an operator reads admin mode into.
fn workable<'a>(snapshot: &'a Snapshot, bloom: &BloomId) -> Option<&'a BloomRecord> {
    snapshot.blooms.get(bloom).filter(|record| matches!(record.status, BloomStatus::Sealed | BloomStatus::Resolved))
}

/// The refusal every admin door shares, checked before anything about the act
/// itself: an unknown bloom and a request that says nothing.
fn common_refusal(record: Option<&BloomRecord>, note: &AdminNote) -> Option<AdminError> {
    if record.is_none() {
        return Some(AdminError::UnknownOrInactiveBloom);
    }
    if note.reason.trim().is_empty() {
        return Some(AdminError::BlankReason);
    }
    if note.operator.trim().is_empty() {
        return Some(AdminError::BlankOperator);
    }
    None
}

/// The record an act inside an open session may run against, or the refusal it
/// earns — the shared ladder every door below the two session doors walks.
fn in_session<'a>(snapshot: &'a Snapshot, bloom: &BloomId, note: &AdminNote) -> Result<&'a BloomRecord, AdminError> {
    let record = workable(snapshot, bloom);
    if let Some(error) = common_refusal(record, note) {
        return Err(error);
    }
    let record = record.ok_or(AdminError::UnknownOrInactiveBloom)?;
    if record.admin.is_none() {
        return Err(AdminError::NotInAdmin);
    }
    // The ADR-0181 line, re-checked here for the reason the two override doors
    // re-check it: admin mode may close findings and place candidates, and a
    // member whose sealed approval resolves above `auto` still needs its signed
    // statement no matter what reason string accompanies the request.
    if let Some(workpiece) = unapproved_member(record) {
        return Err(AdminError::UnapprovedMember(workpiece.clone()));
    }
    Ok(record)
}

/// The refusal a named workpiece earns, or `None` when the record can act on
/// it: it is a member, or it is the reserved composition id.
fn addressable(record: &BloomRecord, workpiece: &WorkpieceId) -> Option<AdminError> {
    let known = workpiece.is_composition() || record.spec.members().iter().any(|member| member.workpiece == *workpiece);
    (!known).then(|| AdminError::NotAWorkpiece(workpiece.clone()))
}

/// The act row plus the outcome that names it — every door's last two lines,
/// written once so a door cannot journal an act under an outcome addressing a
/// different one.
fn acted(bloom: BloomId, kind: AdminActKind, note: &AdminNote, mut effects: Vec<Decision>) -> Decisions {
    let act = AdminAct { kind, note: note.clone() };
    let address = digest_of(&act);
    effects.push(Decision::RecordAdminAct { bloom, act });

    Decisions { outcome: Outcome::AdminActed { bloom, act: address }, effects }
}

fn rejected(error: AdminError) -> Decisions {
    Decisions::rejected(Outcome::AdminRejected(error))
}

/// Reduce an admin-session open ([`Fact::AdminEnter`](crate::Fact::AdminEnter)).
///
/// The flag, plus the brake — except over a bloom an operator has *already*
/// braked, where the existing hold stands untouched. Re-recording it would
/// overwrite the reason that operator gave, which is the one thing whoever
/// finds the frozen bloom is reading it for, and refusing instead would leave a
/// gap between release and enter in which the reactor dispatches the very laps
/// the session exists to stop.
pub(super) fn reduce_admin_enter(snapshot: &Snapshot, bloom: &BloomId, note: &AdminNote) -> Decisions {
    let record = workable(snapshot, bloom);
    if let Some(error) = common_refusal(record, note) {
        return rejected(error);
    }
    let Some(record) = record else {
        return rejected(AdminError::UnknownOrInactiveBloom);
    };
    if record.admin.is_some() {
        return rejected(AdminError::AlreadyInAdmin);
    }

    let mut effects = alloc::vec![Decision::RecordAdminMode { bloom: *bloom, admin: Some(note.clone()) }];
    if record.operator_hold.is_none() {
        effects.push(Decision::RecordOperatorHold { bloom: *bloom, hold: hold_of(note) });
    }

    let act = AdminAct { kind: AdminActKind::Entered, note: note.clone() };
    effects.push(Decision::RecordAdminAct { bloom: *bloom, act });

    Decisions { outcome: Outcome::AdminEntered { bloom: *bloom }, effects }
}

/// Reduce an admin-session close ([`Fact::AdminExit`](crate::Fact::AdminExit)).
///
/// Clears the flag, drops the brake, and re-derives what is due through the
/// release's own helpers — every workpiece whose dispatch the session swallowed
/// from the cursor it sits at *now*, and every aggregate gate the session owes
/// from the fold the record is holding now. Nothing was stored at enter time to
/// be replayed, so a bloom the operator moved by hand resumes from where they
/// left it rather than from where it stopped.
///
/// Then the landing, which is the one thing a plain release never owes: see
/// [`owed_landing`].
pub(super) fn reduce_admin_exit(snapshot: &Snapshot, bloom: &BloomId, note: &AdminNote) -> Decisions {
    let record = workable(snapshot, bloom);
    if let Some(error) = common_refusal(record, note) {
        return rejected(error);
    }
    let Some(record) = record.filter(|record| record.admin.is_some()) else {
        return rejected(AdminError::NotInAdmin);
    };

    let mut effects = alloc::vec![Decision::RecordAdminMode { bloom: *bloom, admin: None }];
    if record.operator_hold.is_some() {
        effects.push(Decision::RecordOperatorRelease { bloom: *bloom, release: hold_of(note) });
    }
    let mut dispatched = Vec::new();
    for workpiece in &record.deferred_dispatches {
        if let Some(owed) = owed_dispatch(record, *bloom, workpiece, snapshot.member_checkpoint(bloom, workpiece)) {
            effects.extend(owed);
            dispatched.push(workpiece.clone());
        }
    }
    effects.extend(owed_aggregates(record, *bloom));

    let landing = owed_landing(record, *bloom, note, &mut effects);
    effects.push(Decision::RecordAdminAct {
        bloom: *bloom,
        act: AdminAct { kind: AdminActKind::Exited, note: note.clone() },
    });

    Decisions { outcome: Outcome::AdminExited { bloom: *bloom, dispatched, landing }, effects }
}

/// Finish the composite-gate join a waiver completed, and say so.
///
/// The narrow case, and every clause is load-bearing. The bloom must still hold
/// a fold; both composite gates must have passed on it; no composition finding
/// may be open; and at least one of those passes must have come from a waiver
/// recorded in this session. Without that last clause exit would resolve blooms
/// the ordinary verdict path was about to resolve on its own, racing it; with
/// it, exit resolves exactly the folds whose completing verdict is never going
/// to arrive because a person stood in for it.
///
/// The [`AdminActKind::LandedOnWaiver`] row is the land record ADR-0219 asks
/// for. It rides the admin log rather than the [`LandingReceipt`] itself, which
/// sits inside `Decision::EmitReceipt` in the frozen decision mirrors and so
/// cannot gain a field without freezing a full copy of every one of them.
fn owed_landing(record: &BloomRecord, bloom: BloomId, note: &AdminNote, effects: &mut Vec<Decision>) -> bool {
    if record.status != BloomStatus::Sealed {
        return false;
    }
    let Some(integration) = record.integration.as_ref() else {
        return false;
    };
    let joined = COMPOSITION_RERUNNABLE.iter().all(|gate| record.aggregate_passed.contains(gate));
    if !joined || record.open_composition_findings().next().is_some() {
        return false;
    }
    let waivers: Vec<Digest> = record.admin_acts.iter().flat_map(AdminAct::waived).copied().collect();
    if waivers.is_empty() {
        return false;
    }

    let head = integration.head;
    effects.push(Decision::RecordAdminAct {
        bloom,
        act: AdminAct { kind: AdminActKind::LandedOnWaiver { head, waivers }, note: note.clone() },
    });
    effects.extend(resolution_effects(record, bloom, integration).1);
    true
}

/// The brake an admin session raises and drops, carrying the session's own
/// words — so a reader of the hold sees the admin reason rather than a
/// synthesized one.
fn hold_of(note: &AdminNote) -> OperatorHold {
    OperatorHold { reason: note.reason.clone(), operator: note.operator.clone() }
}

/// Reduce an admin lane cancellation
/// ([`Fact::AdminCancelLane`](crate::Fact::AdminCancelLane)).
///
/// The cancellation and the record of it, and deliberately nothing else. No
/// cursor moves and no fact is admitted for the cancelled lap, which is what
/// makes this a lane that never ran rather than a lane that failed — the
/// difference between this door and the retry door, which journals an executor
/// fault and spends a machinery roll on purpose.
///
/// The nonce is recorded rather than validated: an order is keyed by nonce in
/// the host's registry, which the reducer cannot read. The REST door resolves
/// it there before admitting, so a nonce reaching here already named a live
/// order of this bloom's.
pub(super) fn reduce_admin_cancel_lane(snapshot: &Snapshot, bloom: &BloomId, cancel: &AdminLaneCancel) -> Decisions {
    let record = match in_session(snapshot, bloom, &cancel.note) {
        Ok(record) => record,
        Err(error) => return rejected(error),
    };
    if let Some(error) = addressable(record, &cancel.workpiece) {
        return rejected(error);
    }

    let effects = alloc::vec![Decision::CancelLane {
        bloom: *bloom,
        workpiece: cancel.workpiece.clone(),
        nonce: cancel.nonce.clone(),
    }];
    let kind = AdminActKind::LaneCancelled { workpiece: cancel.workpiece.clone(), nonce: cancel.nonce.clone() };

    acted(*bloom, kind, &cancel.note, effects)
}

/// Reduce an admin candidate placement
/// ([`Fact::AdminSetCandidate`](crate::Fact::AdminSetCandidate)).
///
/// The repair door without its wedge precondition. A member lands at `Verify`
/// carrying its spent counters forward, and the composition's weave becomes the
/// held integration with both composite gates owed — so the ordinary gates
/// judge what the operator supplied, on exit. What is withheld is only the work
/// order: the cursor moves now, exactly as it does under any other hold.
pub(super) fn reduce_admin_set_candidate(snapshot: &Snapshot, bloom: &BloomId, set: &AdminCandidate) -> Decisions {
    let record = match in_session(snapshot, bloom, &set.note) {
        Ok(record) => record,
        Err(error) => return rejected(error),
    };
    if let Some(error) = addressable(record, &set.workpiece) {
        return rejected(error);
    }
    let kind = AdminActKind::CandidateSet { workpiece: set.workpiece.clone(), candidate: set.candidate };
    if set.workpiece.is_composition() {
        return acted(*bloom, kind, &set.note, rewoven(record, *bloom, set.candidate));
    }

    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == set.workpiece) else {
        return rejected(AdminError::NotAWorkpiece(set.workpiece.clone()));
    };
    let cursor = record.progress.get(&set.workpiece).copied();
    let progress = StageProgress {
        stage: StageId::Verify,
        attempts: 1,
        candidate: Some(set.candidate),
        // Carried, not reset, for the reason `reduce_operator_repair` carries
        // them: an operator writing the candidate buys the member a lap, never
        // a fresh budget, so a bad hand-written fix bounces on the same terms a
        // bad lane's does.
        repair_rolls: cursor.map_or(0, |cursor| cursor.repair_rolls),
        seen_verify_failures: cursor.map_or_else(Default::default, |cursor| cursor.seen_verify_failures),
        fold_checkpoint: cursor.and_then(|cursor| cursor.fold_checkpoint),
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
    };
    let effects = move_effects_with_candidate(
        *bloom,
        &set.workpiece,
        member.scope_revision,
        &progress,
        DispatchTargets { subject: set.candidate.tree, checkout: set.candidate.checkout },
        Some(set.candidate.tree),
        SealedLine::of(record, member),
    );

    acted(*bloom, kind, &set.note, effects.to_vec())
}

/// The composition's half of [`reduce_admin_set_candidate`]: the operator's
/// weave becomes the held integration and both composite gates fall due.
///
/// The lineage the previous fold recorded rides along, because placing a weave
/// edits the composed tree rather than re-ordering what went into it — the same
/// carry `rewoven_by_operator` makes. Both gates are deferred rather than
/// dispatched: admin mode dispatches nothing, and the deferral is what exit
/// reads to send them out. A gate the operator goes on to waive clears its own
/// deferral, so a waived gate is not re-run on the way out.
fn rewoven(record: &BloomRecord, bloom: BloomId, weave: CandidateRef) -> Vec<Decision> {
    alloc::vec![
        Decision::RecordIntegration {
            bloom,
            integration: Some(FoldedIntegration {
                tree: weave.tree,
                head: weave.checkout,
                lineage: record.integration.as_ref().map_or_else(Vec::new, |held| held.lineage.clone()),
            }),
        },
        Decision::AdvanceStage {
            bloom,
            workpiece: WorkpieceId::composition(),
            progress: composition_progress(StageId::Verify, 1, weave),
        },
        Decision::DeferAggregate { bloom, stage: StageId::AggregateVerify },
        Decision::DeferAggregate { bloom, stage: StageId::AggregateReview },
    ]
}

/// Reduce an admin re-run ([`Fact::AdminRerun`](crate::Fact::AdminRerun)).
///
/// One stage, on the candidate the workpiece is already holding, spending
/// nothing. Not the retry door: that one journals an executor fault, which is
/// the operator asserting the stage failed to *judge* its subject and is
/// correctly charged a machinery roll. Here the operator is asserting the
/// record now says something different from what the stage last judged — a
/// withdrawn member, a replaced weave — so there is nothing to charge.
///
/// `now` lifts the session's own brake for this one order and nothing else. The
/// ordinary path defers, and exit dispatches it from wherever the cursor has
/// got to by then.
pub(super) fn reduce_admin_rerun(snapshot: &Snapshot, bloom: &BloomId, rerun: &AdminRerun) -> Decisions {
    let record = match in_session(snapshot, bloom, &rerun.note) {
        Ok(record) => record,
        Err(error) => return rejected(error),
    };
    if let Some(error) = addressable(record, &rerun.workpiece) {
        return rejected(error);
    }
    let kind = AdminActKind::Rerun { workpiece: rerun.workpiece.clone(), stage: rerun.stage, now: rerun.now };
    if rerun.workpiece.is_composition() {
        return match rerun_aggregate(record, *bloom, rerun) {
            Ok(effects) => acted(*bloom, kind, &rerun.note, effects),
            Err(error) => rejected(error),
        };
    }

    if !MEMBER_RERUNNABLE.contains(&rerun.stage) {
        return rejected(AdminError::StageNotRunnable(rerun.stage));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == rerun.workpiece) else {
        return rejected(AdminError::NotAWorkpiece(rerun.workpiece.clone()));
    };
    let Some(cursor) = record.progress.get(&rerun.workpiece).copied() else {
        return rejected(AdminError::NoCursor(rerun.workpiece.clone()));
    };

    let progress = StageProgress { stage: rerun.stage, ..cursor };
    let (targets, construct_checkpoint_base) = reconcile_or_line_targets(
        rerun.stage,
        member.scope_revision,
        super::splice::member_construct_base(record, &rerun.workpiece),
        cursor.candidate,
        cursor.fold_checkpoint.filter(|_| rerun.stage == StageId::Reconcile),
        snapshot.member_checkpoint(bloom, &rerun.workpiece),
    );
    let line = SealedLine::of(record, member);
    let effects = super::attempt::move_effects_with_checkpoint(
        *bloom,
        &rerun.workpiece,
        member.scope_revision,
        &progress,
        (targets, construct_checkpoint_base),
        cursor.candidate.map(|current| current.tree),
        if rerun.now {
            line.released()
        } else {
            line
        },
    );

    acted(*bloom, kind, &rerun.note, effects.to_vec())
}

/// The composition's half of [`reduce_admin_rerun`]: one composite gate over
/// the fold the record is holding.
///
/// No cursor moves. The gates judge the held fold rather than a cursor
/// position, so a re-run that advanced the composition would state a stage
/// transition that did not happen.
fn rerun_aggregate(record: &BloomRecord, bloom: BloomId, rerun: &AdminRerun) -> Result<Vec<Decision>, AdminError> {
    if !COMPOSITION_RERUNNABLE.contains(&rerun.stage) {
        return Err(AdminError::StageNotRunnable(rerun.stage));
    }
    let Some(integration) = record.integration.as_ref() else {
        return Err(AdminError::NoCursor(WorkpieceId::composition()));
    };
    if !rerun.now {
        return Ok(alloc::vec![Decision::DeferAggregate { bloom, stage: rerun.stage }]);
    }

    let dispatch = if rerun.stage == StageId::AggregateVerify {
        owed_aggregate_verify(record, bloom, integration.tree, integration.head, record.aggregate_verify_rolls + 1)
    } else {
        owed_aggregate_review(record, bloom, integration.tree, integration.head, record.aggregate_rolls + 1)
    };

    Ok(alloc::vec![dispatch])
}

/// Reduce an admin waiver ([`Fact::AdminWaive`](crate::Fact::AdminWaive)).
///
/// Three rows and no fourth. The [`Adjudication`] closes the named findings —
/// the ledger entry ADR-0219 asks for, and the reason
/// [`BloomRecord::open_composition_findings`] needs no teaching about waivers.
/// The [`Decision::RecordAggregateGatePass`] records that the gate is satisfied
/// on the fold currently held, which is what makes the verdict count as passed
/// for landing. The admin act says a person made the call, and on what grounds.
///
/// There is no synthesized verdict anywhere in that list. A reader of the
/// journal sees a red verdict, an adjudication naming it, and a gate pass whose
/// only provenance is that adjudication — never a green artifact nobody
/// produced.
pub(super) fn reduce_admin_waive(snapshot: &Snapshot, bloom: &BloomId, waiver: &AdminWaiver) -> Decisions {
    let record = match in_session(snapshot, bloom, &waiver.note) {
        Ok(record) => record,
        Err(error) => return rejected(error),
    };
    if waiver.findings.is_empty() {
        return rejected(AdminError::NoFindings);
    }
    if let Some(stranger) = waiver.findings.iter().find(|finding| !voidable(record, finding)) {
        return rejected(AdminError::UnknownFinding(*stranger));
    }
    if UNVERIFIED_WAIVER_GATES.contains(&waiver.gate) && !waiver.acknowledged_unverified {
        return rejected(AdminError::UnacknowledgedVerifyWaiver(waiver.gate));
    }

    let mut effects = alloc::vec![Decision::RecordAdjudication {
        bloom: *bloom,
        adjudication: Adjudication {
            findings: waiver.findings.clone(),
            disposition: Disposition::Accepted,
            reason: waiver.note.reason.clone(),
            operator: waiver.note.operator.clone(),
        },
    }];
    // A park raised by one of the voided findings is released here for the
    // reason the adjudication door releases it: the waiver *is* the answer, in
    // the one form that carries a reason the landing can quote. A park under
    // some other question stands.
    if let Some(question) = record.review_park.filter(|question| waiver.findings.contains(question)) {
        effects.push(Decision::ReleaseHold { bloom: *bloom, question });
        effects.push(Decision::RecordReviewPark { bloom: *bloom, question: None });
    }
    if COMPOSITION_RERUNNABLE.contains(&waiver.gate) && record.integration.is_some() {
        effects.push(Decision::RecordAggregateGatePass { bloom: *bloom, stage: waiver.gate });
    }
    let kind = AdminActKind::Waived {
        gate: waiver.gate,
        findings: waiver.findings.clone(),
        acknowledged_unverified: waiver.acknowledged_unverified,
    };

    acted(*bloom, kind, &waiver.note, effects)
}

/// Whether `finding` names a verdict this bloom actually raised and has not
/// already closed — the line between voiding a finding and inventing one.
///
/// The same two channels the adjudication door reads: the composition's open
/// findings, and the bloom-scope park's own question, which a legacy or
/// question-raised park carries without a filed finding behind it.
fn voidable(record: &BloomRecord, finding: &Digest) -> bool {
    record.open_composition_findings().any(|open| open.detail == *finding) || record.review_park == Some(*finding)
}

/// Reduce an admin lap drop ([`Fact::AdminDropLap`](crate::Fact::AdminDropLap)).
///
/// The cursor goes back to the candidate it held before the lap, keeping its
/// stage and every spent counter. The lap's evidence stays exactly where it is:
/// a dropped candidate is still something that happened, and deleting its
/// verdict would leave the journal claiming a lap that never ran.
///
/// The nonce is recorded rather than validated, for the reason
/// [`reduce_admin_cancel_lane`] records rather than validates one. What the
/// reducer owns is the revert target, which it reads off
/// [`BloomRecord::displaced_candidates`] — the one-deep undo the cursor fold
/// keeps precisely so this door does not have to trust the request for it.
pub(super) fn reduce_admin_drop_lap(snapshot: &Snapshot, bloom: &BloomId, drop: &AdminLapDrop) -> Decisions {
    let record = match in_session(snapshot, bloom, &drop.note) {
        Ok(record) => record,
        Err(error) => return rejected(error),
    };
    if let Some(error) = addressable(record, &drop.workpiece) {
        return rejected(error);
    }
    let Some(cursor) = record.progress.get(&drop.workpiece).copied() else {
        return rejected(AdminError::NoCursor(drop.workpiece.clone()));
    };
    let (Some(discarded), Some(restored)) =
        (cursor.candidate, record.displaced_candidates.get(&drop.workpiece).copied())
    else {
        return rejected(AdminError::NoLapToDrop(drop.workpiece.clone()));
    };

    let effects = alloc::vec![Decision::AdvanceStage {
        bloom: *bloom,
        workpiece: drop.workpiece.clone(),
        progress: StageProgress { candidate: Some(restored), ..cursor },
    }];
    let kind =
        AdminActKind::LapDropped { workpiece: drop.workpiece.clone(), nonce: drop.nonce.clone(), discarded, restored };

    acted(*bloom, kind, &drop.note, effects)
}

/// Whether this bloom is in a session that absorbs executor faults.
///
/// The hook the two fault reducers call before they spend anything —
/// [`super::attempt::reduce_member_executor_fault`] and
/// [`super::review::reduce_aggregate_review_executor_fault`]. A fault is the
/// host saying it could not run; while an operator is repairing the bloom by
/// hand, a host that cannot run is the expected condition rather than a series
/// worth counting, and charging it would wedge the member the operator is in
/// the middle of fixing.
pub(super) fn absorbs_faults(record: &BloomRecord) -> bool {
    record.admin.is_some()
}

/// What a fault reduces to inside a session: its evidence on the record, and
/// nothing that moves a budget or a cursor.
pub(super) fn absorbed_fault(bloom: BloomId, workpiece: Option<&WorkpieceId>, evidence: &Evidence) -> Decisions {
    Decisions {
        outcome: Outcome::AdminFaultAbsorbed { bloom, workpiece: workpiece.cloned(), evidence: evidence.detail },
        effects: alloc::vec![Decision::RecordEvidence { bloom, evidence: evidence.clone() }],
    }
}
