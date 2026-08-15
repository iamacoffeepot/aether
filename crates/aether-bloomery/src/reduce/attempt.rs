//! The per-member line: evaluating one attempt's completion gate and deciding
//! advance / retry / repair-re-entry / wedge (ADR-0149 §The line, ADR-0153).

use alloc::vec::Vec;

use super::composition::reduce_composition_attempt;
use super::integrate::claim_effects;
use super::splice::member_construct_base;
use super::verify_memo::reuse_of;
use super::{AttemptCompletedError, BloomRecord, BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{
    CandidateRef, ConfigRegistry, Evidence, Membership, ResolutionClaim, StageBinding, StageCatalog, Transformation,
    VerifyFailureSet, Wedge,
};

/// The move-and-dispatch effect pair every cursor move of
/// [`reduce_attempt_completed`] emits — an advance, a Refine re-entry, and a
/// same-stage retry all land the cursor at `progress` and dispatch the stage it
/// names against the member's current targets (`subject` binds the returned
/// evidence, `checkout` is the commit the worker checks out, ADR-0152).
///
/// Under an operator hold the second effect is a [`Decision::DeferDispatch`]
/// instead; see [`move_effects_with_candidate`].
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
///
/// **The one place a [`Decision::DispatchAttempt`] is built** — every route
/// into the line comes through here or through [`move_effects`], which
/// delegates to it: a seal-time or readiness entry, an advance, a retry, a
/// Refine re-entry, a reconcile, a grant, an operator repair, and the
/// composition's weave repair. That is what makes the operator hold (#4976)
/// one guard rather than a policy scattered over eight call sites, and what
/// makes the guard hard to forget: the flag rides on [`SealedLine`], the
/// only way to reach this function.
///
/// Held, the pair becomes the advance plus a [`Decision::DeferDispatch`]: the
/// cursor still moves (the fact that produced it reduces and journals exactly as
/// it always did) and the work order is simply not written. A first seal
/// constructs [`SealedLine`] with `held: false` because a hold names an
/// existing bloom, and a seal is what brings one into existence. A dependent
/// that becomes ready later reads the live record, so a hold taken in the
/// meantime swallows that entry the same way it swallows every other move.
pub(super) fn move_effects_with_candidate(
    bloom: BloomId,
    workpiece: &WorkpieceId,
    scope_revision: Digest,
    progress: StageProgress,
    targets: DispatchTargets,
    candidate: Option<Digest>,
    sealed: SealedLine<'_>,
) -> [Decision; 2] {
    let advance = Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress };
    if sealed.held {
        return [advance, Decision::DeferDispatch { bloom, workpiece: workpiece.clone() }];
    }
    let binding = stage_binding(sealed.catalog, progress.stage);

    [
        advance,
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
/// flattened configuration registry it resolves through, the stage catalog that
/// calibrates it, the base its candidate is built over — and whether that bloom
/// is currently on the operator brake (#4976). All four travel together; a
/// first seal assembles them from the spec (no record exists yet) and every
/// later move reads them off the record.
pub(super) struct SealedLine<'a> {
    /// The member's registry layered over the bloom's.
    pub configs: ConfigRegistry,
    /// The git commit this member's candidate is built over — the bloom's
    /// sealed base for a root, or the spliced dependency tip for a dependent
    /// (ADR-0196). The mechanical `Verify` lane diffs against this range.
    pub base: Digest,
    /// The catalog the bloom sealed, or the compiled line when it sealed none.
    pub catalog: &'a StageCatalog,
    /// Whether an operator has frozen this bloom's dispatch (#4976). Carried
    /// here rather than passed alongside so it cannot be omitted: every
    /// constructor reads it off the record, and this value is the only way into
    /// [`move_effects_with_candidate`].
    pub held: bool,
}

impl<'a> SealedLine<'a> {
    /// The line one member of `record` dispatches under. Every field is read
    /// off the record and the membership, so a call site cannot assemble two
    /// of the four from the bloom and the rest from somewhere else.
    pub(super) fn of(record: &'a BloomRecord, member: &Membership) -> Self {
        Self {
            configs: member.configs.layered_over(record.spec.configs()),
            catalog: &record.stage_catalog,
            base: member_construct_base(record, &member.workpiece),
            held: record.operator_hold.is_some(),
        }
    }

    /// The same line, read as the release itself will leave the record (#4976).
    ///
    /// The one caller is
    /// [`reduce_operator_release`](super::operator_hold::reduce_operator_release),
    /// and it needs this because the reducer is pure: it decides against the
    /// record as it stands, where the hold is still set, while the effects it
    /// returns include the [`Decision::RecordOperatorRelease`] that clears it.
    /// Without lifting the flag here the release would defer the very dispatches
    /// it exists to emit. Named rather than assembled inline so the exemption is
    /// one greppable call with one caller, instead of a `held: false` literal
    /// that reads like an oversight.
    pub(super) fn released(mut self) -> Self {
        self.held = false;
        self
    }
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
    // The composition workpiece is a subject like a member but not *of* the
    // membership, so it is routed before the member lookup that would otherwise
    // refuse it as a stranger (ADR-0191).
    if workpiece.is_composition() {
        return reduce_composition_attempt(snapshot, bloom, stage, passed, evidence, captured);
    }
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
    // repair-only `Refine` and the fold-conflict `Reconcile` sit off the
    // standing line (ADR-0153 / ADR-0189) with an explicit successor. A
    // Reconcile that assembled a dependent's base (no prior candidate, no
    // claim) returns to Construct so the member builds on the spliced tree
    // rather than verifying the assembly as if it were their work. Every
    // other Reconcile — and Refine — returns to Verify for the delta-confirm.
    let assembling = stage == StageId::Reconcile
        && record.progress.get(workpiece).is_some_and(|cursor| cursor.candidate.is_none())
        && !record.claims.contains_key(workpiece);
    let next = if stage == StageId::Refine || (stage == StageId::Reconcile && !assembling) {
        Some(StageId::Verify)
    } else if stage == StageId::Reconcile {
        Some(StageId::Construct)
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
    // The member's candidate after this completion (ADR-0152): a passing attempt
    // adopts the capture it carried (a mechanical lane carries none — the prior
    // candidate rides forward); a failing attempt adopts nothing, so its capture
    // is discarded and the member stays at the candidate its last pass produced.
    // A base-assembly Reconcile is the exception: its capture *is* the spliced
    // base. It rides as the cursor candidate so Construct checks it out, but
    // `fold_checkpoint` stays the collision head so a standing-head re-collision
    // still wedges (#4952).
    let prior = cursor.candidate;
    let candidate = if passed {
        captured.or(prior)
    } else {
        prior
    };
    // The dispatch targets re-resolve from the cursor (ADR-0152): with a
    // candidate present, the returned evidence binds its tree and the worker
    // checks out its capture commit; without one, the member's frozen scope
    // revision and the spliced construct base (ADR-0196). Reconcile is the
    // exception: a *retry* checks out the folded checkpoint the collision
    // named (ADR-0189). A pass leaves that checkout — Verify retargets from
    // the new candidate like any other advance. A passing base-assembly
    // Reconcile checks out the assembled capture as Construct's base.
    let fold_checkpoint = cursor.fold_checkpoint.filter(|_| stage == StageId::Reconcile && !passed);
    let construct_base = member_construct_base(record, workpiece);
    let targets = if assembling && passed {
        DispatchTargets {
            subject: member.scope_revision,
            checkout: candidate.map_or(construct_base, |current| current.checkout),
        }
    } else {
        reconcile_or_line_targets(member.scope_revision, construct_base, candidate, fold_checkpoint)
    };
    let ctx = CompletionCtx { bloom: *bloom, workpiece, member, cursor: &cursor, candidate, targets };
    let effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    if let Some(next) = next.filter(|_| passed) {
        return advance_after_pass(snapshot, record, next, &ctx, effects);
    }
    retry_or_wedge(record, stage, &ctx, evidence, effects)
}

#[derive(Clone, Copy)]
struct CompletionCtx<'a> {
    bloom: BloomId,
    workpiece: &'a WorkpieceId,
    member: &'a Membership,
    cursor: &'a StageProgress,
    candidate: Option<CandidateRef>,
    targets: DispatchTargets,
}

fn advance_after_pass(
    snapshot: &Snapshot,
    record: &BloomRecord,
    next: StageId,
    ctx: &CompletionCtx<'_>,
    mut effects: Vec<Decision>,
) -> Decisions {
    let CompletionCtx { bloom, workpiece, member, cursor, candidate, targets } = *ctx;
    // The fold round outlives the stage (#4952): a reconciled candidate has not
    // folded yet when its lane passes, so the checkpoint it was reconciled onto
    // stays on the cursor until the fold either takes the candidate or moves.
    // The conflict evidence does not — it is the wedge attachment for the
    // Reconcile stage this pass just left.
    let progress = StageProgress {
        stage: next,
        attempts: 1,
        candidate,
        repair_rolls: cursor.repair_rolls,
        seen_verify_failures: cursor.seen_verify_failures,
        fold_checkpoint: cursor.fold_checkpoint,
        fold_conflict_evidence: None,
    };
    // The member may be advancing onto a tree this bloom already proved
    // (#4891) — a repair lap that changed nothing the tree records hands
    // back the candidate its last verify passed. Pass by identity: the
    // member lands on the claim a dispatched pass would have produced,
    // carrying the same verdict, and the mechanical lane never runs.
    if let Some((current, proof)) = candidate
        .filter(|_| next == StageId::Verify)
        .and_then(|current| record.verify_proof_for(current.tree).map(|proof| (current, proof)))
    {
        let claim = ResolutionClaim {
            workpiece: workpiece.clone(),
            scope_revision: member.scope_revision,
            candidate: current.tree,
            evidence: proof.evidence.clone(),
        };
        let reuse = reuse_of(bloom, StageId::Verify, proof);
        effects.push(Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress });
        effects.extend(claim_effects(snapshot, record, bloom, &claim, Some(reuse)));
        return Decisions {
            outcome: Outcome::VerifyReused { bloom, workpiece: workpiece.clone(), proof: proof.evidence.detail },
            effects,
        };
    }

    effects.extend(move_effects_with_candidate(
        bloom,
        workpiece,
        member.scope_revision,
        progress,
        targets,
        candidate.map(|current| current.tree),
        SealedLine::of(record, member),
    ));
    Decisions {
        outcome: Outcome::AttemptAdvanced { bloom, workpiece: workpiece.clone(), from: cursor.stage, to: next },
        effects,
    }
}

fn retry_or_wedge(
    record: &BloomRecord,
    stage: StageId,
    ctx: &CompletionCtx<'_>,
    evidence: &Evidence,
    mut effects: Vec<Decision>,
) -> Decisions {
    let CompletionCtx { bloom, workpiece, member, cursor, candidate, targets } = *ctx;
    let fold_conflict_evidence = cursor.fold_conflict_evidence.filter(|_| stage == StageId::Reconcile);
    let budget = record.stage_catalog.retry_budget_of(stage).unwrap_or(1);
    if cursor.attempts < budget {
        let attempt = cursor.attempts + 1;
        let progress = StageProgress {
            stage,
            attempts: attempt,
            candidate,
            repair_rolls: cursor.repair_rolls,
            seen_verify_failures: cursor.seen_verify_failures,
            fold_checkpoint: cursor.fold_checkpoint,
            fold_conflict_evidence,
        };
        effects.extend(move_effects_with_candidate(
            bloom,
            workpiece,
            member.scope_revision,
            progress,
            targets,
            candidate.map(|current| current.tree),
            SealedLine::of(record, member),
        ));
        return Decisions {
            outcome: Outcome::AttemptRetried { bloom, workpiece: workpiece.clone(), stage, attempt },
            effects,
        };
    }
    // Reconcile exhaustion attaches the collision evidence, not the last
    // attempt's — the operator (and a later grant) needs the paths that
    // started the stage, not the lane's most recent miss.
    let mut wedge_evidence = evidence.clone();
    if let Some(detail) = fold_conflict_evidence {
        wedge_evidence.detail = detail;
    }
    wedged(bloom, workpiece, stage, &wedge_evidence, effects)
}

/// Dispatch targets for a member-line move, or the folded checkpoint when
/// the member is reconciling a collision (ADR-0189).
pub(super) fn reconcile_or_line_targets(
    scope_revision: Digest,
    base: Digest,
    candidate: Option<CandidateRef>,
    fold_checkpoint: Option<Digest>,
) -> DispatchTargets {
    if let Some(checkpoint) = fold_checkpoint {
        return DispatchTargets {
            subject: candidate.map_or(scope_revision, |current| current.tree),
            checkout: checkpoint,
        };
    }
    candidate.map_or(DispatchTargets { subject: scope_revision, checkout: base }, |current| DispatchTargets {
        subject: current.tree,
        checkout: current.checkout,
    })
}

/// The terminal answer for a member that has spent `stage`'s retry budget: stop
/// dispatching, and record why.
///
/// The outcome alone reaches only the caller of the fact that wedged it. The
/// record is what every later reader sees — the outward view, an operator, the
/// next person asking why a bloom stopped — and the stage cursor cannot stand in
/// for it, since a member exhausted at `Verify` and one mid-flight on its last
/// roll carry the same cursor.
pub(super) fn wedged(
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
