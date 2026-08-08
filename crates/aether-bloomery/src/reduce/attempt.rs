//! The per-member line: evaluating one attempt's completion gate and deciding
//! advance / retry / repair-re-entry / wedge (ADR-0149 §The line, ADR-0153).

use super::{AttemptCompletedError, BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{AgentProfile, CandidateRef, ConfigRegistry, Evidence, StageCatalog, Transformation};

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
    [
        Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress },
        Decision::DispatchAttempt {
            bloom,
            workpiece: workpiece.clone(),
            stage: progress.stage,
            transformation: Transformation::for_member_stage(progress.stage, targets.subject, targets.checkout),
            scope_revision,
            candidate: progress.candidate.map(|current| current.tree),
            profile: stage_profile(sealed.catalog, progress.stage),
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
    /// The catalog the bloom sealed, or the compiled line when it sealed none.
    pub catalog: &'a StageCatalog,
}

/// The profile a catalog calibrates one stage at, falling back to the compiled
/// line when the catalog binds no such stage.
///
/// The fallback is unreachable for any catalog a bloom actually runs — the seal
/// door refuses one that leaves a stage unbound — so it exists to keep the
/// dispatch total rather than to express a policy. Dispatching *something* the
/// operator would recognize beats a panic in the one path that has no way to
/// report a refusal.
pub(super) fn stage_profile(catalog: &StageCatalog, stage: StageId) -> AgentProfile {
    catalog.profile_for(stage).cloned().unwrap_or_else(|| StageCatalog::profile_of(stage))
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
/// `retry_budget` allows and wedges the member once it is exhausted (a wedged
/// member stops dispatching — a supersession is the escape). The attempt's
/// evidence is recorded in the bloom's evidence log either way. The terminal
/// `Verify` is the exception to same-stage retry: a *passing* `Verify` integrates
/// the member through [`Fact::Integrate`](crate::Fact::Integrate) and never completes here (a passing
/// terminal completion is a mis-route,
/// [`AttemptCompletedError::TerminalStage`]); a *failing* `Verify` re-enters
/// `Refine` — the findings-directed fix, since re-running the mechanical gate on
/// an unchanged candidate changes nothing — bounded by Verify's `retry_budget`
/// over the cursor-carried `repair_rolls`, wedging once the budget's worth of
/// failing verdicts is consumed.
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
    // A *passing* terminal `Verify` is a mis-route — a passing Verify integrates
    // through `Fact::Integrate` and never completes here, so a passing completion
    // whose stage has no successor is rejected. A *failing* `Verify` does complete
    // here (the Refine re-entry below), so the guard fires only on the passing
    // terminal case; a mis-routed passing terminal is caught before the cursor
    // check so it reads as `TerminalStage` rather than a `StageMismatch`. The
    // repair-only `Refine` sits off the standing line (ADR-0153) with an explicit
    // successor: its pass returns the member to `Verify` for the delta-confirm.
    let next = if stage == StageId::Refine {
        Some(StageId::Verify)
    } else {
        StageCatalog::next_member_stage(stage)
    };
    if passed && next.is_none() {
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
    if let Some(next) = next.filter(|_| passed) {
        let progress = StageProgress { stage: next, attempts: 1, candidate, repair_rolls };
        effects.extend(move_effects(
            *bloom,
            workpiece,
            member.scope_revision,
            progress,
            DispatchTargets { subject, checkout },
            SealedLine { configs: member.configs.layered_over(record.spec.configs()), catalog: &record.stage_catalog },
        ));
        return Decisions {
            outcome: Outcome::AttemptAdvanced { bloom: *bloom, workpiece: workpiece.clone(), from: stage, to: next },
            effects,
        };
    }
    // A failing terminal Verify re-enters Refine instead of re-running the
    // mechanical gate on an unchanged candidate (ADR-0153): only a
    // findings-directed fix changes the next verdict, so the member routes back
    // to the repair stage that can produce one (the host threads the persisted
    // failure findings onto the dispatch, #3656). The ceiling is Verify's retry
    // budget over `repair_rolls` — the cursor-carried count the per-stage
    // `attempts` reset cannot clear — so once the budget's worth of failing
    // verdicts is consumed the member wedges: never an extra roll, never a
    // silent integrate.
    if stage == StageId::Verify {
        let rolls = repair_rolls + 1;
        if rolls < record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1) {
            let progress = StageProgress { stage: StageId::Refine, attempts: 1, candidate, repair_rolls: rolls };
            effects.extend(move_effects(
                *bloom,
                workpiece,
                member.scope_revision,
                progress,
                DispatchTargets { subject, checkout },
                SealedLine {
                    configs: member.configs.layered_over(record.spec.configs()),
                    catalog: &record.stage_catalog,
                },
            ));
            return Decisions {
                outcome: Outcome::RefineReentered { bloom: *bloom, workpiece: workpiece.clone(), rolls },
                effects,
            };
        }
        return Decisions {
            outcome: Outcome::AttemptWedged { bloom: *bloom, workpiece: workpiece.clone(), stage },
            effects,
        };
    }
    // A failing gate re-dispatches the same stage while its retry budget allows;
    // an exhausted budget wedges the member — it stops dispatching rather than
    // looping (the tripwire).
    let budget = record.stage_catalog.retry_budget_of(stage).unwrap_or(1);
    if attempts < budget {
        let attempt = attempts + 1;
        let progress = StageProgress { stage, attempts: attempt, candidate, repair_rolls };
        effects.extend(move_effects(
            *bloom,
            workpiece,
            member.scope_revision,
            progress,
            DispatchTargets { subject, checkout },
            SealedLine { configs: member.configs.layered_over(record.spec.configs()), catalog: &record.stage_catalog },
        ));
        return Decisions {
            outcome: Outcome::AttemptRetried { bloom: *bloom, workpiece: workpiece.clone(), stage, attempt },
            effects,
        };
    }
    Decisions { outcome: Outcome::AttemptWedged { bloom: *bloom, workpiece: workpiece.clone(), stage }, effects }
}
