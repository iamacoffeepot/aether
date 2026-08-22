//! Arm of [`super::reduce`]'s fact dispatch (`Fact::GrantAttempts`); wiring
//! lives in `mod.rs`.
//!
//! The wedged-member escape hatch that does not mint a bloom: handing a member
//! back attempts on the bloom it already belongs to (#4708).
//!
//! A wedge is a fact about execution, not about sealed work. The spec was fine;
//! the attempts ran out. Superseding models that as a *new identity*, which
//! forces an operator to fabricate a content difference — a perturbed scope
//! revision — to express an execution decision, and throws away the candidate
//! the wedged member had already built. This door expresses it directly.

use super::attempt::{SealedLine, move_effects_with_checkpoint, reconcile_or_line_targets};
use super::{BloomStatus, Decision, Decisions, GrantAttemptsError, Outcome, Snapshot, StageProgress};
use crate::ids::{BloomId, StageId, WorkpieceId};

/// Grant a wedged member `attempts` more attempts and re-dispatch it
/// ([`Fact::GrantAttempts`](crate::Fact::GrantAttempts)).
///
/// The grant moves the member's cursor, and a cursor that moves is a member that
/// is dispatching again — so it needs no clear-the-wedge concept of its own:
/// [`Decision::AdvanceStage`] is already the only route out of the wedged set,
/// and this emits the same advance-and-dispatch pair every other cursor move
/// emits.
///
/// **Which counter it hands back depends on the wedge cause (#5091), not only
/// the stage.** A `MACHINERY` wedge exhausted the independent host-fault series
/// — no judgment was rendered — so the grant resets only that series, leaves
/// `attempts` / `repair_rolls` / the candidate alone, and resumes at the wedged
/// stage itself. Re-entering `Refine` would spend a model lap against an
/// unchanged candidate, which is the thing ADR-0195 routes away from. A `WORK`
/// wedge keeps today's axis: a wedge at `Verify` spent cursor-carried
/// `repair_rolls`, so the grant lowers those and resumes at `Refine`; every
/// other work wedge lowers `attempts` and resumes in place.
///
/// Either way `attempts` means the same thing to the operator asking for it:
/// **how many more dispatched attempts the member may spend on that axis before
/// it wedges again**. The counters are set to leave exactly that much headroom
/// under the stage's own budget, which is also the ceiling — a member cannot be
/// handed back more than its stage was ever calibrated to spend. One grant is
/// one sealed-budget-sized batch; another batch is another grant with a fresh
/// idempotency key after the next wedge.
pub(super) fn reduce_grant_attempts(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    attempts: u32,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::GrantAttemptsRejected(GrantAttemptsError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::GrantAttemptsRejected(GrantAttemptsError::UnknownOrInactiveBloom));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::GrantAttemptsRejected(GrantAttemptsError::NotAMember(workpiece.clone())));
    };
    // Only a wedged member is grantable. A running member already has attempts
    // and re-dispatching it would put two workers on one workpiece; a member that
    // has never dispatched (an inherited claim, which holds no cursor) has no
    // position to resume from. Both refuse here rather than fabricating one.
    let Some(wedge) = record.wedged.get(workpiece) else {
        return Decisions::rejected(Outcome::GrantAttemptsRejected(GrantAttemptsError::NotWedged(workpiece.clone())));
    };
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::GrantAttemptsRejected(GrantAttemptsError::NotWedged(workpiece.clone())));
    };
    // The grant names the stage it believes the member is stuck at. A mismatch is
    // an operator acting on a stale read of the projection — the member wedged
    // somewhere else — so it is refused rather than silently applied to whatever
    // the record says now.
    if wedge.stage != stage {
        return Decisions::rejected(Outcome::GrantAttemptsRejected(GrantAttemptsError::StageMismatch {
            wedged_at: wedge.stage,
            got: stage,
        }));
    }
    // The stage's own retry budget is the hard ceiling, and the only one: the
    // counters this grant writes are read against it, so a larger request could
    // not be spent even if it were admitted. The sealed catalog is where retry
    // authority lives (ADR-0177), so there is no second bloom-wide cap to
    // reconcile it with.
    let budget = record.stage_catalog.retry_budget_of(stage).unwrap_or(1);
    if attempts == 0 || attempts > budget {
        return Decisions::rejected(Outcome::GrantAttemptsRejected(GrantAttemptsError::BeyondCap {
            requested: attempts,
            cap: budget,
        }));
    }
    // The dispatch re-targets from the cursor exactly as a retry does (ADR-0152):
    // with a candidate present the returned evidence binds its tree and the worker
    // checks out its capture commit, so the resumed attempt continues the work the
    // wedged member had already built rather than starting from the sealed base.
    // Without a candidate, a construct checkpoint seeds the checkout the same
    // way a same-stage retry does (#4994).
    let candidate = cursor.candidate;
    let fold_checkpoint = cursor.fold_checkpoint.filter(|_| stage == StageId::Reconcile);
    let (targets, construct_checkpoint_base) = reconcile_or_line_targets(
        member.scope_revision,
        super::splice::member_construct_base(record, workpiece),
        candidate,
        fold_checkpoint,
        snapshot.member_checkpoint(bloom, workpiece),
    );
    // A machinery series at the sealed ceiling is the #5091 cause: the host
    // never judged the candidate. Anything else is the work/repair axis.
    let machinery = snapshot
        .member_machinery(bloom, workpiece)
        .is_some_and(|fault| fault.stage == wedge.stage && fault.rolls >= budget);
    let progress = if machinery {
        // Same stage, same candidate, same work/repair counters. Headroom
        // lives on the machinery series, written after the cursor move so
        // the grant's AdvanceStage (which retires a spent series) does not
        // wipe the leftover rolls.
        cursor
    } else if stage == StageId::Verify {
        StageProgress {
            stage: StageId::Refine,
            attempts: 1,
            candidate,
            repair_rolls: budget - attempts,
            seen_verify_failures: cursor.seen_verify_failures,
            fold_checkpoint: cursor.fold_checkpoint,
            fold_conflict_evidence: None,
            reconcile_assembles_base: false,
        }
    } else {
        StageProgress {
            stage,
            attempts: budget + 1 - attempts,
            candidate,
            repair_rolls: cursor.repair_rolls,
            seen_verify_failures: cursor.seen_verify_failures,
            fold_checkpoint: cursor.fold_checkpoint,
            fold_conflict_evidence: cursor.fold_conflict_evidence,
            reconcile_assembles_base: false,
        }
    };
    let mut effects = move_effects_with_checkpoint(
        *bloom,
        workpiece,
        member.scope_revision,
        progress,
        (targets, construct_checkpoint_base),
        candidate.map(|current| current.tree),
        SealedLine::of(record, member),
    )
    .to_vec();
    if machinery {
        let spent = budget - attempts;
        if spent > 0 {
            effects.push(Decision::RecordMemberMachinery {
                bloom: *bloom,
                workpiece: workpiece.clone(),
                stage,
                rolls: spent,
                evidence: wedge.evidence,
            });
        }
    }
    Decisions {
        outcome: Outcome::AttemptsGranted {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            resumes_at: progress.stage,
            attempts,
        },
        effects,
    }
}
