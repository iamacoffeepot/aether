//! The wedged-member escape hatch that does not mint a bloom: handing a member
//! back attempts on the bloom it already belongs to (#4708).
//!
//! A wedge is a fact about execution, not about sealed work. The spec was fine;
//! the attempts ran out. Superseding models that as a *new identity*, which
//! forces an operator to fabricate a content difference — a perturbed scope
//! revision — to express an execution decision, and throws away the candidate
//! the wedged member had already built. This door expresses it directly.

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate};
use super::{BloomStatus, Decisions, GrantAttemptsError, Outcome, Snapshot, StageProgress};
use crate::ids::{BloomId, StageId, WorkpieceId};

/// Grant a wedged member `attempts` more attempts and re-dispatch it
/// ([`Fact::GrantAttempts`](crate::Fact::GrantAttempts)).
///
/// The grant moves the member's cursor, and a cursor that moves is a member that
/// is dispatching again — so it needs no clear-the-wedge concept of its own:
/// [`Decision::AdvanceStage`](crate::Decision::AdvanceStage) is already the only
/// route out of the wedged set, and this emits the same advance-and-dispatch
/// pair every other cursor move emits.
///
/// **Which counter it hands back depends on the stage.** A wedge at `Verify` is
/// not spent `attempts` — repeated verifier failures spend cursor-carried
/// `repair_rolls`, which the per-stage `attempts` reset cannot clear — so a
/// Verify grant lowers `repair_rolls` and resumes at `Refine`, the re-entry the
/// wedge denied. Resuming at `Verify` instead would re-run the mechanical gate
/// on an unchanged candidate, which is the thing ADR-0153 routes away from: the
/// verdict cannot change until a findings-directed fix changes the candidate.
/// Every other stage wedges on `attempts` against its own budget, so a grant
/// there lowers `attempts` and resumes in place.
///
/// Either way `attempts` means the same thing to the operator asking for it:
/// **how many more dispatched attempts the member may spend before it wedges
/// again**. The counters are set to leave exactly that much headroom under the
/// stage's own budget, which is also the ceiling — a member cannot be handed
/// back more than its stage was ever calibrated to spend.
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
    let candidate = cursor.candidate;
    let (subject, checkout) = candidate
        .map_or_else(|| (member.scope_revision, record.spec.base()), |current| (current.tree, current.checkout));
    // Leave exactly `attempts` of headroom under the stage's budget. `Verify`
    // counts repair rolls and resumes at `Refine`; every other stage counts
    // attempts and resumes in place.
    let progress = if stage == StageId::Verify {
        StageProgress {
            stage: StageId::Refine,
            attempts: 1,
            candidate,
            repair_rolls: budget - attempts,
            seen_verify_failures: cursor.seen_verify_failures,
        }
    } else {
        StageProgress {
            stage,
            attempts: budget + 1 - attempts,
            candidate,
            repair_rolls: cursor.repair_rolls,
            seen_verify_failures: cursor.seen_verify_failures,
        }
    };
    let effects = move_effects_with_candidate(
        *bloom,
        workpiece,
        member.scope_revision,
        progress,
        DispatchTargets { subject, checkout },
        candidate.map(|current| current.tree),
        SealedLine {
            configs: member.configs.layered_over(record.spec.configs()),
            catalog: &record.stage_catalog,
            base: record.spec.base(),
        },
    );
    Decisions {
        outcome: Outcome::AttemptsGranted {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            resumes_at: progress.stage,
            attempts,
        },
        effects: effects.to_vec(),
    }
}
