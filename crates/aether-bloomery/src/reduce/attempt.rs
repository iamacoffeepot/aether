//! The per-member line: evaluating one attempt's completion gate and deciding
//! advance / retry / repair-re-entry / wedge (ADR-0149 §The line, ADR-0153).

use alloc::vec::Vec;

use super::{AttemptCompletedError, BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{
    CandidateRef, ConfigRegistry, Evidence, StageBinding, StageCatalog, Transformation, VerifyFailureSet, Wedge,
};

/// The move-and-dispatch effect pair every cursor move of
/// [`reduce_attempt_completed`] emits — an advance, a Refine re-entry, and a
/// same-stage retry all land the cursor at `progress` and dispatch the stage it
/// names against the member's current targets (`subject` binds the returned
/// evidence, `checkout` is the commit the worker checks out, ADR-0152).
pub(super) fn move_effects(
    bloom: BloomId,
    workpiece: &WorkpieceId,
    scope_revision: Digest,
    progress: StageProgress,
    targets: DispatchTargets,
    sealed: SealedLine<'_>,
) -> [Decision; 2] {
    move_effects_with_candidate(
        bloom,
        workpiece,
        scope_revision,
        progress,
        targets,
        progress.candidate.map(|current| current.tree),
        sealed,
    )
}

/// Compose a cursor move whose displayed candidate can outlive the optional
/// checkout pair retained on the cursor. Aggregate repair re-entry uses this
/// when an inherited resolution claim names a candidate tree but the member
/// has no candidate-bearing cursor.
pub(super) fn move_effects_with_candidate(
    bloom: BloomId,
    workpiece: &WorkpieceId,
    scope_revision: Digest,
    progress: StageProgress,
    targets: DispatchTargets,
    candidate: Option<Digest>,
    sealed: SealedLine<'_>,
) -> [Decision; 2] {
    let binding = stage_binding(sealed.catalog, progress.stage);

    [
        Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress },
        Decision::DispatchAttempt {
            bloom,
            workpiece: workpiece.clone(),
            stage: progress.stage,
            transformation: Transformation::for_member_stage(&binding, targets.subject, targets.checkout, sealed.base),
            scope_revision,
            candidate,
            profile: binding.profile,
            configs: sealed.configs,
        },
    ]
}

/// The two independent digests one dispatch aims at (ADR-0152). Paired because
/// they move together per stage and are easy to transpose: `subject` binds the
/// returned evidence, `checkout` is the git commit the worker checks out, and
/// swapping them dispatches work against the wrong tree while binding evidence to
/// something no one built.
#[derive(Clone, Copy)]
pub(super) struct DispatchTargets {
    /// The digest the returned evidence must bind to.
    pub subject: Digest,
    /// The git commit the attempt's worker checks out.
    pub checkout: Digest,
}

/// What a dispatch inherits from the bloom that sealed it (ADR-0174): the
/// flattened configuration registry it resolves through, and the stage catalog
/// that calibrates it. Both come off the bloom's record, so they travel together.
pub(super) struct SealedLine<'a> {
    /// The member's registry layered over the bloom's.
    pub configs: ConfigRegistry,
    /// The git commit the bloom was sealed onto — the base every member's
    /// candidate is built over, and so the range the mechanical `Verify` lane
    /// reads its candidate's diff against (#4890).
    pub base: Digest,
    /// The catalog the bloom sealed, or the compiled line when it sealed none.
    pub catalog: &'a StageCatalog,
}

/// The binding a catalog gives one stage, falling back to the compiled line's
/// when the catalog binds no such stage.
///
/// One resolution per dispatch, because one binding answers everything a
/// dispatch asks of the catalog: the profile the attempt runs under and the
/// wall-clock limit it runs within. Resolving them separately would let a
/// dispatch pair one stage's calibration with another's limit.
///
/// The fallback is unreachable for any catalog a bloom actually runs — the seal
/// door refuses one that leaves a stage unbound — so it exists to keep the
/// dispatch total rather than to express a policy. Dispatching *something* the
/// operator would recognize beats a panic in the one path that has no way to
/// report a refusal.
pub(super) fn stage_binding(catalog: &StageCatalog, stage: StageId) -> StageBinding {
    catalog.binding(stage).cloned().unwrap_or_else(|| StageCatalog::binding_of(stage))
}

/// Reduce a per-member attempt completion (ADR-0149 §The line,
/// [`Fact::AttemptCompleted`](crate::Fact::AttemptCompleted)).
///
/// The reducer alone advances line position: it reads the member's cursor,
/// evaluates the stage's completion gate against the host-reported `passed`
/// signal, and decides advance / retry / wedge — the host submits transformations
/// and reports raw outcomes but never advances state (the ADR-0149 invariant, and
/// the reason the "host evaluates the gate" alternative was rejected).
///
/// A passing gate advances the cursor to the next member stage and dispatches it
/// (a passing repair-only `Refine` returns to `Verify` for the delta-confirm,
/// ADR-0153); a failing gate re-dispatches the same stage while the stage's
/// `retry_budget` allows and wedges the member once it is exhausted. The
/// terminal `Verify` never completes here: a pass integrates through
/// [`Fact::Integrate`](crate::Fact::Integrate), while a failure carries its typed
/// identities through [`Fact::VerifyFailed`](crate::Fact::VerifyFailed).
pub(super) fn reduce_attempt_completed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    passed: bool,
    evidence: &Evidence,
    captured: Option<CandidateRef>,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::UnknownOrInactiveBloom));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::NotAMember(
            workpiece.clone(),
        )));
    };
    // Terminal `Verify` is a mis-route in either direction: passes integrate and
    // failures use the typed VerifyFailed fact. It is caught before the cursor
    // check so it reads as `TerminalStage` rather than a `StageMismatch`. The
    // repair-only `Refine` sits off the standing line (ADR-0153) with an explicit
    // successor: its pass returns the member to `Verify` for the delta-confirm.
    let next = if stage == StageId::Refine {
        Some(StageId::Verify)
    } else {
        StageCatalog::next_member_stage(stage)
    };
    if next.is_none() {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::TerminalStage(stage)));
    }
    // The completion must name the member's current cursor stage. A member with
    // no cursor never entered the dispatched line (it arrived as an inherited
    // claim), which is its own refusal — not a mismatch against a fabricated
    // entry-stage cursor (#3663); a result for a stage the member has already
    // left is stale/out-of-order and is not acted on.
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::NotDispatched(
            workpiece.clone(),
        )));
    };
    if cursor.stage != stage {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::StageMismatch {
            expected: cursor.stage,
            got: stage,
        }));
    }
    let attempts = cursor.attempts;
    // The member's candidate after this completion (ADR-0152): a passing attempt
    // adopts the capture it carried (a mechanical lane carries none — the prior
    // candidate rides forward); a failing attempt adopts nothing, so its capture
    // is discarded and the member stays at the candidate its last pass produced.
    let prior = cursor.candidate;
    let candidate = if passed {
        captured.or(prior)
    } else {
        prior
    };
    // The dispatch targets re-resolve from the cursor (ADR-0152): with a
    // candidate present, the returned evidence binds its tree and the worker
    // checks out its capture commit; without one, the member's frozen scope
    // revision and the bloom's sealed base (ADR-0149 §Execution, #3572).
    let (subject, checkout) = candidate
        .map_or_else(|| (member.scope_revision, record.spec.base()), |current| (current.tree, current.checkout));
    // The attempt result is journaled evidence about the member, recorded whatever
    // the gate decides.
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    // A passing gate advances the cursor to the next member stage and dispatches
    // it. `next` is `Some` on this branch — a passing terminal completion was
    // rejected above, so a passing stage always has a successor.
    let repair_rolls = cursor.repair_rolls;
    let seen_verify_failures = cursor.seen_verify_failures;
    if let Some(next) = next.filter(|_| passed) {
        let progress = StageProgress { stage: next, attempts: 1, candidate, repair_rolls, seen_verify_failures };
        effects.extend(move_effects_with_candidate(
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
        ));
        return Decisions {
            outcome: Outcome::AttemptAdvanced { bloom: *bloom, workpiece: workpiece.clone(), from: stage, to: next },
            effects,
        };
    }
    // A failing gate re-dispatches the same stage while its retry budget allows;
    // an exhausted budget wedges the member — it stops dispatching rather than
    // looping (the tripwire).
    let budget = record.stage_catalog.retry_budget_of(stage).unwrap_or(1);
    if attempts < budget {
        let attempt = attempts + 1;
        let progress = StageProgress { stage, attempts: attempt, candidate, repair_rolls, seen_verify_failures };
        effects.extend(move_effects_with_candidate(
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
        ));
        return Decisions {
            outcome: Outcome::AttemptRetried { bloom: *bloom, workpiece: workpiece.clone(), stage, attempt },
            effects,
        };
    }
    wedged(*bloom, workpiece, stage, evidence, effects)
}

/// The terminal answer for a member that has spent `stage`'s retry budget: stop
/// dispatching, and record why.
///
/// The outcome alone reaches only the caller of the fact that wedged it. The
/// record is what every later reader sees — the outward view, an operator, the
/// next person asking why a bloom stopped — and the stage cursor cannot stand in
/// for it, since a member exhausted at `Verify` and one mid-flight on its last
/// roll carry the same cursor.
fn wedged(
    bloom: BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    evidence: &Evidence,
    mut effects: Vec<Decision>,
) -> Decisions {
    effects.push(Decision::RecordWedge {
        bloom,
        workpiece: workpiece.clone(),
        wedge: Wedge { stage, evidence: evidence.detail, repeated_verifiers: VerifyFailureSet::EMPTY },
    });
    Decisions {
        outcome: Outcome::AttemptWedged {
            bloom,
            workpiece: workpiece.clone(),
            stage,
            repeated_verifiers: VerifyFailureSet::EMPTY,
        },
        effects,
    }
}
