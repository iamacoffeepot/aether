//! The operator brake (#4976): freezing one bloom's dispatch, and letting it go
//! again.
//!
//! The rest of the operator vocabulary acts on a bloom that has already run out
//! of road — [`super::operator`] adjudicates the findings a parked composition
//! could not repair, [`super::grant`] hands a wedged member more attempts. None
//! of them is the move for a bloom that is *running* and looks wrong. Until this
//! door the only brake was killing the coordinator, which stops the laps
//! mid-flight and re-runs them on the next boot: an operator who wanted to stop
//! spending had to pay for the stop.
//!
//! So the hold is deliberately narrow.
//!
//! - **It gates dispatch, and only dispatch.** While a bloom is held the reducer
//!   emits no [`Decision::DispatchAttempt`], [`Decision::DispatchAggregateVerify`],
//!   or [`Decision::DispatchAggregateReview`] for it. Every other fact — a lane
//!   completing, a verify verdict, a fold outcome, a landing refusal — reduces
//!   exactly as it always did, so the work already running finishes and lands in
//!   the journal. That is the whole difference between a hold and a kill. The
//!   two aggregate gates ride their own decision paths, so they need the same
//!   brake the member line already had: an operator stopping a bloom that is
//!   producing bad composition candidates must not pay for a verify or a
//!   critic of each one (#5100).
//! - **It touches nothing else.** No claim is released, no budget is spent or
//!   handed back, no finding is opened or closed, no approval tier moves. It
//!   composes with the review park rather than replacing it: holding a parked
//!   bloom leaves the park exactly where it was, and releasing a bloom whose
//!   review parked it does not answer the park — the bloom comes off the brake
//!   and stops again on its own question, which is the correct outcome for two
//!   independent reasons to stop.
//! - **It is bloom-level and flat.** One hold per bloom, no per-member scope, no
//!   priority, no timed resume. A hold that could express more would need a
//!   policy to resolve it against, and the whole value of this door is that it
//!   needs none.
//!
//! The release stores nothing and replays nothing. It reads the record as it
//! stands and re-derives each owed member dispatch from the cursor it finds —
//! the same move [`super::grant`] makes when it resumes a wedged member,
//! through the same two helpers — and each owed aggregate gate from the fold
//! the record is holding. So a bloom that moved on in some other way while it
//! was held dispatches from where it actually is, and a dispatch can be
//! neither lost nor doubled.

use alloc::vec::Vec;

use super::aggregate_verify::{owed_aggregate_review, owed_aggregate_verify};
use super::attempt::{
    DispatchTargets, SealedLine, move_effects_with_candidate, move_effects_with_checkpoint, reconcile_or_line_targets,
};
use super::composition::composition_line;
use super::{BloomRecord, BloomStatus, Decision, Decisions, OperatorHoldError, Outcome, Snapshot};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{CandidateRef, OperatorHold};

/// The bloom both doors act on: known, and still running a line worth freezing.
///
/// `Sealed` is the ordinary case and `Resolved` is included because a bloom
/// awaiting its landing can still be sent back into the line by a refused
/// landing, which is exactly the spend an operator watching a red landing branch
/// wants to stop. A landed or superseded bloom dispatches nothing there is
/// anything left to freeze.
fn brakeable<'a>(snapshot: &'a Snapshot, bloom: &BloomId) -> Option<&'a BloomRecord> {
    snapshot.blooms.get(bloom).filter(|record| matches!(record.status, BloomStatus::Sealed | BloomStatus::Resolved))
}

/// The refusals both doors share: an unknown bloom, and a request that says
/// nothing.
///
/// Deliberately *not* shared with the [`super::operator`] doors is their
/// ADR-0181 approval re-check. That guard exists because an adjudication and a
/// repair each carry a bloom *toward* a landing, and an override must not become
/// the route by which unapproved work lands. Neither edge of a hold moves a
/// bloom anywhere: one stops dispatch and the other resumes the dispatch the
/// seal door already approved. Refusing to let an operator stop a bloom whose
/// membership is unapproved would refuse the brake precisely where it is most
/// wanted.
fn refusal(record: Option<&BloomRecord>, request: &OperatorHold) -> Option<OperatorHoldError> {
    if record.is_none() {
        return Some(OperatorHoldError::UnknownOrInactiveBloom);
    }
    if request.reason.trim().is_empty() {
        return Some(OperatorHoldError::BlankReason);
    }
    if request.operator.trim().is_empty() {
        return Some(OperatorHoldError::BlankOperator);
    }
    None
}

/// Reduce an operator hold ([`Fact::OperatorHold`](crate::Fact::OperatorHold)).
///
/// One effect: the flag. Nothing is captured at this moment to be replayed at
/// release — the dispatches the hold is about to swallow have not happened yet,
/// and the ones already in flight are the host's, not the reducer's to recall.
pub(super) fn reduce_operator_hold(snapshot: &Snapshot, bloom: &BloomId, hold: &OperatorHold) -> Decisions {
    let record = brakeable(snapshot, bloom);
    if let Some(error) = refusal(record, hold) {
        return Decisions::rejected(Outcome::OperatorHoldRejected(error));
    }
    // Holding a held bloom is refused rather than absorbed: a second hold would
    // journal a fact that changed nothing, and it would overwrite the reason the
    // first one recorded — which is the one thing whoever finds the frozen bloom
    // is reading it for.
    if record.is_some_and(|record| record.operator_hold.is_some()) {
        return Decisions::rejected(Outcome::OperatorHoldRejected(OperatorHoldError::AlreadyHeld));
    }

    Decisions {
        outcome: Outcome::BloomHeld { bloom: *bloom },
        effects: alloc::vec![Decision::RecordOperatorHold { bloom: *bloom, hold: hold.clone() }],
    }
}

/// Reduce an operator release ([`Fact::OperatorRelease`](crate::Fact::OperatorRelease)).
///
/// Clears the flag, then re-derives every dispatch the hold swallowed from the
/// cursor that workpiece is sitting at now — and every aggregate gate the hold
/// swallowed from the fold the record is holding now.
///
/// The member set it walks is [`BloomRecord::deferred_dispatches`] rather than
/// every cursor on the record, and that is the whole correctness argument. A
/// workpiece whose worker is still running holds the same cursor as one whose
/// dispatch was swallowed, so dispatching from every cursor would put a second
/// worker on every lap that outlived the hold. Walking only the deferrals
/// dispatches exactly what is owed. The aggregate set is the same argument at
/// bloom scope: a fold whose verify is still in flight looks like one whose
/// verify was withheld, so only a recorded deferral is owed.
///
/// What is re-derived, rather than recalled, is the dispatch itself: targets off
/// the cursor, catalog and profile and configuration off the record. So a
/// workpiece the hold caught at `Construct` and that a later fact moved to
/// `Reconcile` dispatches the reconcile, not the stale construct.
///
/// A workpiece that *wedged* under the hold is owed nothing, and needs no check
/// here: the wedge dropped it from the set as it was recorded, because a release
/// that handed back the lap a wedge had just refused would be a retry grant
/// wearing a different name.
pub(super) fn reduce_operator_release(snapshot: &Snapshot, bloom: &BloomId, release: &OperatorHold) -> Decisions {
    let record = brakeable(snapshot, bloom);
    if let Some(error) = refusal(record, release) {
        return Decisions::rejected(Outcome::OperatorHoldRejected(error));
    }
    let Some(record) = record.filter(|record| record.operator_hold.is_some()) else {
        return Decisions::rejected(Outcome::OperatorHoldRejected(OperatorHoldError::NotHeld));
    };

    let mut effects = alloc::vec![Decision::RecordOperatorRelease { bloom: *bloom, release: release.clone() }];
    let mut dispatched = Vec::new();
    for workpiece in &record.deferred_dispatches {
        if let Some(owed) = owed_dispatch(record, *bloom, workpiece, snapshot.member_checkpoint(bloom, workpiece)) {
            effects.extend(owed);
            dispatched.push(workpiece.clone());
        }
    }
    effects.extend(owed_aggregates(record, *bloom));

    Decisions { outcome: Outcome::BloomReleased { bloom: *bloom, dispatched }, effects }
}

/// The aggregate work orders a release owes, re-derived from the fold the
/// record is holding now.
///
/// Every gate the hold swallowed goes back out, because the two composite gates
/// run concurrently against one fold: neither reads the other's verdict, and
/// the landing waits on the join of their passes. Releasing only one would
/// leave the bloom waiting on a gate nothing re-dispatches. A gate whose work
/// order actually goes out clears its own deferral in the fold — the same
/// implicit clear a member dispatch uses.
pub(super) fn owed_aggregates(record: &BloomRecord, bloom: BloomId) -> Vec<Decision> {
    let Some(integration) = record.integration.as_ref() else {
        return Vec::new();
    };
    let mut owed = Vec::new();
    if record.deferred_aggregates.contains(&StageId::AggregateVerify) {
        owed.push(owed_aggregate_verify(
            record,
            bloom,
            integration.tree,
            integration.head,
            record.aggregate_verify_rolls + 1,
        ));
    }
    if record.deferred_aggregates.contains(&StageId::AggregateReview) {
        owed.push(owed_aggregate_review(record, bloom, integration.tree, integration.head, record.aggregate_rolls + 1));
    }
    owed
}

/// Which of the two dispatch brakes an owed-dispatch door is entitled to lift.
///
/// The two release doors each lift the brake they themselves clear in the same
/// decision set. A host-fault resume clears neither.
#[derive(Clone, Copy)]
enum LiftedBrake {
    /// The operator hold. [`owed_dispatch`] for [`reduce_operator_release`].
    Operator,
    /// The unproven-base brake. [`owed_base_dispatch`] for a green receipt.
    Base,
    /// Neither. [`owed_resume_dispatch`] for a host-fault resume.
    Neither,
}

impl LiftedBrake {
    fn apply(self, line: SealedLine<'_>) -> SealedLine<'_> {
        match self {
            Self::Operator => line.released(),
            Self::Base => line.base_released(),
            Self::Neither => line,
        }
    }
}

/// The advance-and-dispatch pair one owed workpiece is due, aimed from the
/// cursor it currently holds — or `None` when the record holds no position to
/// dispatch it from.
///
/// The door is [`reduce_operator_release`]: it lifts the operator brake because
/// the same decision set records the release that clears it.
///
/// The cursor is re-emitted unchanged: a release buys no attempt and spends
/// none, so the only thing that moves is the work order the hold withheld. The
/// targets come from the same [`reconcile_or_line_targets`] the grant door uses,
/// for the same reason — with a candidate present the returned evidence binds
/// its tree and the worker checks out its capture commit, a construct
/// checkpoint seeds the checkout when there is no candidate (#4994), and a
/// member in a fold round checks out the collision head instead.
///
/// The `None` arms are unreachable under today's transitions: a deferral is only
/// ever recorded beside a cursor move, for a workpiece that is a member or the
/// composition, and nothing removes a cursor from a live record. They are stated
/// so an impossible entry is dropped rather than dispatched from a position the
/// reducer invented — an owed dispatch nobody can aim is a stuck workpiece an
/// operator can see, and a misaimed one is a worker building the wrong tree.
pub(super) fn owed_dispatch(
    record: &BloomRecord,
    bloom: BloomId,
    workpiece: &WorkpieceId,
    member_checkpoint: Option<CandidateRef>,
) -> Option<[Decision; 2]> {
    owed_dispatch_lifted(record, bloom, workpiece, member_checkpoint, LiftedBrake::Operator)
}

/// The same re-derived dispatch as [`owed_dispatch`], lifting the base brake
/// rather than the operator brake.
///
/// The door is a green base receipt ([`super::base_verify`]): it must not lift
/// an operator hold.
pub(super) fn owed_base_dispatch(
    record: &BloomRecord,
    bloom: BloomId,
    workpiece: &WorkpieceId,
    member_checkpoint: Option<CandidateRef>,
) -> Option<[Decision; 2]> {
    owed_dispatch_lifted(record, bloom, workpiece, member_checkpoint, LiftedBrake::Base)
}

/// The same re-derived dispatch as [`owed_dispatch`], lifting neither brake.
///
/// The door is a host-fault resume
/// ([`super::verify::reduce_resume_host_fault`]): it clears the host condition,
/// not an operator hold or an unproven base. A held record therefore defers the
/// re-probe, and the release that actually lifts the brake is the one that later
/// dispatches it.
pub(super) fn owed_resume_dispatch(
    record: &BloomRecord,
    bloom: BloomId,
    workpiece: &WorkpieceId,
    member_checkpoint: Option<CandidateRef>,
) -> Option<[Decision; 2]> {
    owed_dispatch_lifted(record, bloom, workpiece, member_checkpoint, LiftedBrake::Neither)
}

fn owed_dispatch_lifted(
    record: &BloomRecord,
    bloom: BloomId,
    workpiece: &WorkpieceId,
    member_checkpoint: Option<CandidateRef>,
    lift: LiftedBrake,
) -> Option<[Decision; 2]> {
    let cursor = record.progress.get(workpiece).copied()?;
    let candidate = cursor.candidate;
    if workpiece.is_composition() {
        // The composition dispatches against its own weave, under the bloom-wide
        // line rather than a member-layered one, exactly as its weave repair
        // does.
        let weave = candidate?;
        return Some(move_effects_with_candidate(
            bloom,
            workpiece,
            record.spec.base(),
            cursor,
            DispatchTargets { subject: weave.tree, checkout: weave.checkout },
            Some(weave.tree),
            lift.apply(composition_line(record)),
        ));
    }
    let member = record.spec.members().iter().find(|member| member.workpiece == *workpiece)?;
    let (targets, construct_checkpoint_base) = reconcile_or_line_targets(
        member.scope_revision,
        super::splice::member_construct_base(record, workpiece),
        candidate,
        cursor.fold_checkpoint.filter(|_| cursor.stage == StageId::Reconcile),
        member_checkpoint,
    );

    Some(move_effects_with_checkpoint(
        bloom,
        workpiece,
        member.scope_revision,
        cursor,
        (targets, construct_checkpoint_base),
        candidate.map(|current| current.tree),
        lift.apply(SealedLine::of(record, member)),
    ))
}
