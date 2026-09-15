//! The per-member line: evaluating one attempt's completion gate and deciding
//! advance / retry / repair-re-entry / wedge (ADR-0149 §The line, ADR-0153).

use alloc::vec::Vec;

use super::composition::reduce_composition_attempt;
use super::coordination::carried_head_pin;
use super::integrate::claim_effects;
use super::splice::member_construct_base;
use super::verify_memo::reuse_of;
use super::{
    AttemptCompletedError, BloomRecord, BloomStatus, Decision, Decisions, MemberExecutorFaultError, Outcome, Snapshot,
    StageProgress,
};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{
    CandidateRef, ConfigRegistry, Evidence, EvidenceKind, Membership, ResolutionClaim, StageBinding, StageCatalog,
    Transformation, VerifyFailureSet, Wedge,
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
    progress: &StageProgress,
    targets: DispatchTargets,
    sealed: SealedLine<'_>,
) -> [Decision; 2] {
    move_effects_with_checkpoint(
        bloom,
        workpiece,
        scope_revision,
        progress,
        (targets, None),
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
    progress: &StageProgress,
    targets: DispatchTargets,
    candidate: Option<Digest>,
    sealed: SealedLine<'_>,
) -> [Decision; 2] {
    move_effects_with_checkpoint(bloom, workpiece, scope_revision, progress, (targets, None), candidate, sealed)
}

/// The same builder as [`move_effects_with_candidate`], carrying Construct
/// checkpoint provenance when `reconcile_or_line_targets` selected one.
///
/// `construct_checkpoint_base` is the member's clean or spliced base. It
/// becomes [`Transformation::diff_base`] only on Construct — the local
/// backend turns that marker into `--seeded` and withholds it from
/// `--diff-base` (#5052). Verify names its range through
/// [`Transformation::for_member_stage`] as [`SealedLine::base`], which is
/// the member's own construct base — a dependent spliced onto an ancestor
/// is judged only on the files it changed. Aggregate verify still diffs
/// the woven tree against the bloom's sealed base.
pub(super) fn move_effects_with_checkpoint(
    bloom: BloomId,
    workpiece: &WorkpieceId,
    scope_revision: Digest,
    progress: &StageProgress,
    (targets, construct_checkpoint_base): (DispatchTargets, Option<Digest>),
    candidate: Option<Digest>,
    sealed: SealedLine<'_>,
) -> [Decision; 2] {
    let advance = Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress: *progress };
    if sealed.withheld() {
        return [advance, Decision::DeferDispatch { bloom, workpiece: workpiece.clone() }];
    }
    let binding = stage_binding(sealed.catalog, progress.stage);
    let subject = if progress.stage == StageId::Construct {
        scope_revision
    } else {
        targets.subject
    };
    let mut transformation = Transformation::for_member_stage(&binding, subject, targets.checkout, sealed.base);
    if progress.stage == StageId::Construct
        && let Some(base) = construct_checkpoint_base
    {
        transformation.diff_base = Some(base);
    }
    let candidate = if progress.stage == StageId::Construct {
        None
    } else {
        candidate
    };

    [
        advance,
        Decision::DispatchAttempt {
            bloom,
            workpiece: workpiece.clone(),
            stage: progress.stage,
            transformation,
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
    /// Whether this bloom's sealed base holds a green whole-workspace receipt
    /// (ADR-0200). Independent of [`Self::held`]: releasing one brake must not
    /// lift the other.
    pub base_proven: bool,
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
            base_proven: record.base_proven,
        }
    }

    /// Whether either brake is on: the operator hold, or an unproven base.
    pub(super) fn withheld(&self) -> bool {
        self.held || !self.base_proven
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
    /// that reads like an oversight. Clears only the operator half: releasing
    /// one brake must not lift the other.
    pub(super) fn released(mut self) -> Self {
        self.held = false;
        self
    }

    /// The same line, read as a green base receipt will leave the record
    /// (ADR-0200).
    ///
    /// The reducer decides against the record as it stands, where
    /// `base_proven` is still false, while the effects it returns include the
    /// [`Decision::RecordBaseReceipt`] that sets it. Clears only the base
    /// half: releasing one brake must not lift the other.
    pub(super) fn base_released(mut self) -> Self {
        self.base_proven = true;
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
/// `retry_budget` allows and wedges the member once it is exhausted. A failing
/// Construct whose evidence is [`EvidenceKind::ConstructDeclined`] parks
/// instead: attempts and repair rolls stay put — except a Reconcile decline on
/// work the head already carries with a standing claim, which resolves as
/// current rather than parking a member that has nothing left to do. The terminal `Verify` never
/// completes here: a pass integrates through
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
        return reduce_composition_attempt(snapshot, bloom, workpiece, stage, passed, evidence, captured);
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
    // stage check so it reads as `TerminalStage` rather than a `StageMismatch`.
    // The repair-only `Refine` and the fold-conflict `Reconcile` sit off the
    // standing line (ADR-0153 / ADR-0189) with an explicit successor. A
    // Reconcile that assembled a dependent's base (`reconcile_assembles_base`,
    // recorded by `Fact::FoldConflict` when there was no prior candidate and no
    // claim) returns to Construct so the member builds on the spliced tree
    // rather than verifying the assembly as if it were their work. Every
    // other Reconcile — and Refine — returns to Verify for the delta-confirm.
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::NotDispatched(
            workpiece.clone(),
        )));
    };
    let assembling = cursor.reconcile_assembles_base;
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
    if cursor.stage != stage {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::StageMismatch {
            expected: cursor.stage,
            got: stage,
        }));
    }
    // A construct that concluded without a candidate is not a failed attempt:
    // the lane finished its reasoning and refused to produce work. Parking
    // spends nothing; retrying would reproduce the same refusal against the
    // same inputs (#5292). A dead construct keeps `VerificationResult` and
    // still takes `retry_or_wedge` below — that is the case a second attempt
    // can recover.
    // The declining lane is `construct.implement`, which `StageCatalog` maps
    // from Construct, Refine and Reconcile alike (`LaneGates::of` keys
    // `is_construct` on the command), so a declining repair lap parks the same
    // way a first construct does rather than falling into `retry_or_wedge`.
    if !passed
        && matches!(stage, StageId::Construct | StageId::Refine | StageId::Reconcile)
        && evidence.kind == EvidenceKind::ConstructDeclined
    {
        if stage == StageId::Reconcile
            && let Some(resolved) = resolve_covered_reconcile(record, member, &cursor, *bloom, workpiece, evidence)
        {
            return resolved;
        }
        return park_declined_construct(
            *bloom,
            workpiece,
            stage,
            evidence,
            alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }],
        );
    }
    // The member's candidate after this completion (ADR-0152): a passing attempt
    // adopts the capture it carried (a mechanical lane carries none — the prior
    // candidate rides forward); a failing attempt adopts nothing onto the
    // cursor, so the member stays at the candidate its last pass produced and
    // the retry still checks out that tree. A failing construct's capture is
    // not discarded — `Snapshot::apply` records it as the member's newest
    // checkpoint, keyed on `passed: false`, without touching this cursor, and
    // the retry checks that commit out (#4994) while still binding the scope
    // revision. A base-assembly Reconcile is the exception: its capture *is*
    // the spliced base. It rides as the cursor candidate so Construct checks
    // it out, but `fold_checkpoint` stays the collision head so a standing-head
    // re-collision still wedges (#4952).
    let prior = cursor.candidate;
    let candidate = if passed {
        captured.or(prior)
    } else {
        prior
    };
    // The dispatch targets re-resolve from the cursor (ADR-0152): with a
    // candidate present, the returned evidence binds its tree and the worker
    // checks out its capture commit; without one, the newest construct
    // checkpoint (#4994) or the spliced construct base (ADR-0196). Reconcile
    // is the exception: a *retry* checks out the folded checkpoint the
    // collision named (ADR-0189). A pass leaves that checkout — Verify
    // retargets from the new candidate like any other advance. A passing
    // base-assembly Reconcile checks out the assembled capture as Construct's
    // base. This completion's own failing construct capture is newer than
    // anything already on the snapshot — `apply` has not recorded it yet.
    let fold_checkpoint = cursor.fold_checkpoint.filter(|_| stage == StageId::Reconcile && !passed);
    let construct_base = member_construct_base(record, workpiece);
    let member_checkpoint = captured
        .filter(|_| stage == StageId::Construct && !passed)
        .or_else(|| snapshot.member_checkpoint(bloom, workpiece));
    let target_stage = if passed {
        next.unwrap_or(stage)
    } else {
        stage
    };
    let target_fold = if passed {
        None
    } else {
        fold_checkpoint
    };
    let (targets, construct_checkpoint_base) = reconcile_or_line_targets(
        target_stage,
        member.scope_revision,
        construct_base,
        candidate,
        target_fold,
        member_checkpoint,
    );
    let ctx = CompletionCtx {
        bloom: *bloom,
        workpiece,
        member,
        cursor: &cursor,
        candidate,
        targets,
        construct_checkpoint_base,
    };
    let effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    if let Some(next) = next.filter(|_| passed) {
        return advance_after_pass(snapshot, record, next, &ctx, effects);
    }
    retry_or_wedge(record, stage, &ctx, evidence, effects)
}

/// Reduce a member-stage executor environment fault (ADR-0195) — the dispatched
/// gate reporting that it could not judge the member at all.
///
/// A branch entirely separate from [`reduce_attempt_completed`] and
/// [`super::verify::reduce_verify_failed`], because nothing here is a verdict
/// about a candidate: the cursor stays put, the candidate stays put, and neither
/// `attempts` nor `repair_rolls` move. The fault records against the member's
/// current stage and, while the sealed stage budget allows, redispatches the
/// *same* artifact under a fresh order through [`reconcile_or_line_targets`].
/// At the ceiling it records a wedge whose cause the projection reads as
/// machinery.
pub(super) fn reduce_member_executor_fault(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    evidence: &Evidence,
) -> Decisions {
    let ctx = match FaultCtx::admissible(snapshot, bloom, workpiece, stage, evidence) {
        Ok(ctx) => ctx,
        Err(refusal) => return Decisions::rejected(Outcome::MemberExecutorFaultRejected(refusal)),
    };

    let rolls = match snapshot.member_machinery(bloom, workpiece) {
        Some(fault) if fault.stage == stage => fault.rolls.saturating_add(1),
        _ => 1,
    };
    let budget = ctx.record.stage_catalog.retry_budget_of(stage).unwrap_or(1);
    let mut effects = alloc::vec![
        Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() },
        Decision::RecordMemberMachinery {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            stage,
            rolls,
            evidence: evidence.detail,
        },
    ];

    if rolls >= budget {
        effects.push(Decision::RecordWedge {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge { stage, evidence: evidence.detail, repeated_verifiers: VerifyFailureSet::EMPTY },
        });
        return Decisions {
            outcome: Outcome::MachineryWedged { bloom: *bloom, workpiece: workpiece.clone(), stage, rolls, budget },
            effects,
        };
    }

    effects.extend(if ctx.cursor.stage == stage {
        redispatch_current_stage(&ctx)
    } else {
        hand_held_candidate_to_stage(&ctx)
    });
    Decisions {
        outcome: Outcome::MachineryRetried { bloom: *bloom, workpiece: workpiece.clone(), stage, rolls, budget },
        effects,
    }
}

/// Everything the two redispatch shapes read, resolved once by
/// [`FaultCtx::admissible`].
#[derive(Clone, Copy)]
struct FaultCtx<'a> {
    snapshot: &'a Snapshot,
    record: &'a BloomRecord,
    bloom: &'a BloomId,
    workpiece: &'a WorkpieceId,
    member: &'a Membership,
    /// The member's cursor as the fault found it. `cursor.stage` is the stage
    /// the member sits at; [`Self::stage`] is the stage the fault names, and
    /// the two differ only on the held-candidate hand-off.
    cursor: StageProgress,
    stage: StageId,
}

impl<'a> FaultCtx<'a> {
    /// Resolve the fault against the snapshot, or name the refusal that keeps
    /// it out.
    ///
    /// The stage gate is the door #5969 opens: ordinarily a fault must name the
    /// stage the cursor sits at, because a fault aimed from a stale read of the
    /// board would otherwise spend a roll on the wrong stage. A member *holding
    /// a candidate* is the one shape where that is demonstrably not a stale
    /// read — the capture exists, and `Verify` is the only stage that judges it
    /// — so naming `Verify` over a held candidate is accepted and hands the
    /// capture to its verify rather than wedging the member behind another
    /// construct lap.
    fn admissible(
        snapshot: &'a Snapshot,
        bloom: &'a BloomId,
        workpiece: &'a WorkpieceId,
        stage: StageId,
        evidence: &Evidence,
    ) -> Result<Self, MemberExecutorFaultError> {
        let record = snapshot
            .blooms
            .get(bloom)
            .filter(|record| record.status == BloomStatus::Sealed)
            .ok_or(MemberExecutorFaultError::UnknownOrInactiveBloom)?;
        let member = record
            .spec
            .members()
            .iter()
            .find(|member| member.workpiece == *workpiece)
            .ok_or_else(|| MemberExecutorFaultError::NotAMember(workpiece.clone()))?;
        let cursor = record
            .progress
            .get(workpiece)
            .copied()
            .ok_or_else(|| MemberExecutorFaultError::NotDispatched(workpiece.clone()))?;

        let holds_a_capture_to_verify = stage == StageId::Verify && cursor.candidate.is_some();
        if cursor.stage != stage && !holds_a_capture_to_verify {
            return Err(MemberExecutorFaultError::StageMismatch { expected: cursor.stage, got: stage });
        }

        let subject = cursor.candidate.map_or_else(|| member.scope_revision, |current| current.tree);
        if !evidence.validates(&subject) {
            return Err(MemberExecutorFaultError::EvidenceNotBound { expected: subject, got: evidence.subject });
        }

        Ok(Self { snapshot, record, bloom, workpiece, member, cursor, stage })
    }
}

/// Redispatch the member's current stage over the same artifact under a fresh
/// order: nothing about the cursor moves, so the cursor itself is the progress
/// the move carries. A Construct order displays no candidate — the lane is
/// building one — while every later stage displays the capture it judges.
fn redispatch_current_stage(ctx: &FaultCtx<'_>) -> [Decision; 2] {
    let FaultCtx { snapshot, record, bloom, workpiece, member, cursor, stage } = *ctx;
    let targets = reconcile_or_line_targets(
        stage,
        member.scope_revision,
        member_construct_base(record, workpiece),
        cursor.candidate,
        cursor.fold_checkpoint.filter(|_| stage == StageId::Reconcile),
        snapshot.member_checkpoint(bloom, workpiece),
    );
    let displayed = if stage == StageId::Construct {
        None
    } else {
        cursor.candidate.map(|current| current.tree)
    };

    move_effects_with_checkpoint(
        *bloom,
        workpiece,
        member.scope_revision,
        &cursor,
        targets,
        displayed,
        SealedLine::of(record, member),
    )
}

/// Move the cursor to `ctx.stage` carrying the capture it already holds — the
/// hand-off [`FaultCtx::admissible`] lets through.
///
/// A fresh stage entry, so `attempts` restarts at one, and the fold round
/// outlives the stage exactly as it does on a passing advance (#4952): the
/// checkpoint the capture was reconciled onto rides along until the fold either
/// takes the candidate or moves, while the conflict evidence does not — that
/// was the wedge attachment of the Reconcile stage this capture has left.
fn hand_held_candidate_to_stage(ctx: &FaultCtx<'_>) -> [Decision; 2] {
    let FaultCtx { snapshot, record, bloom, workpiece, member, cursor, stage } = *ctx;
    let progress = StageProgress {
        stage,
        attempts: 1,
        candidate: cursor.candidate,
        repair_rolls: cursor.repair_rolls,
        seen_verify_failures: cursor.seen_verify_failures,
        fold_checkpoint: cursor.fold_checkpoint,
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
    };
    let targets = reconcile_or_line_targets(
        stage,
        member.scope_revision,
        member_construct_base(record, workpiece),
        cursor.candidate,
        None,
        snapshot.member_checkpoint(bloom, workpiece),
    );

    move_effects_with_checkpoint(
        *bloom,
        workpiece,
        member.scope_revision,
        &progress,
        targets,
        cursor.candidate.map(|current| current.tree),
        SealedLine::of(record, member),
    )
}

#[derive(Clone, Copy)]
struct CompletionCtx<'a> {
    bloom: BloomId,
    workpiece: &'a WorkpieceId,
    member: &'a Membership,
    cursor: &'a StageProgress,
    candidate: Option<CandidateRef>,
    targets: DispatchTargets,
    construct_checkpoint_base: Option<Digest>,
}

fn advance_after_pass(
    snapshot: &Snapshot,
    record: &BloomRecord,
    next: StageId,
    ctx: &CompletionCtx<'_>,
    mut effects: Vec<Decision>,
) -> Decisions {
    let CompletionCtx { bloom, workpiece, member, cursor, candidate, targets, construct_checkpoint_base } = *ctx;
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
        reconcile_assembles_base: false,
    };
    // The member may be advancing onto a tree this bloom already proved
    // (#4891) — a repair lap that changed nothing the tree records hands
    // back the candidate its last verify passed. Pass by identity: the
    // member lands on the claim a dispatched pass would have produced,
    // carrying the same verdict, and the mechanical lane never runs.
    if let Some((current, proof)) = candidate
        .filter(|_| next == StageId::Verify)
        .and_then(|current| record.verify_proof_for(StageId::Verify, current.tree).map(|proof| (current, proof)))
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

    let displayed = if next == StageId::Construct {
        None
    } else {
        candidate.map(|current| current.tree)
    };
    effects.extend(move_effects_with_checkpoint(
        bloom,
        workpiece,
        member.scope_revision,
        &progress,
        (targets, construct_checkpoint_base),
        displayed,
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
    let CompletionCtx { bloom, workpiece, member, cursor, candidate, targets, construct_checkpoint_base } = *ctx;
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
            reconcile_assembles_base: cursor.reconcile_assembles_base,
        };
        let displayed = if stage == StageId::Construct {
            None
        } else {
            candidate.map(|current| current.tree)
        };
        effects.extend(move_effects_with_checkpoint(
            bloom,
            workpiece,
            member.scope_revision,
            &progress,
            (targets, construct_checkpoint_base),
            displayed,
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

/// Resolve a Reconcile decline as current when the declined lap, the standing
/// claim, and the head all name the same member version's content.
///
/// A reconcile lap that finds the head already carrying its subject concludes
/// with nothing to produce — the lane's decline. Retrying reproduces the
/// refusal, and parking strands the cursor at Reconcile while the claim
/// stands, which the console reads as stuck. When the member's claim is
/// recorded at the version the head carries, there is nothing to repair and
/// nowhere to repair it onto: the cursor rejoins the line at Verify on the
/// carried candidate, with no dispatch — the work the head holds is already
/// proven by the standing claim.
///
/// Anything less than the full triangle still parks: an uncovered member, a
/// claim for another version, or a lap for another version is genuine
/// divergence the reducer cannot verify from digests, and the park keeps it
/// visible.
fn resolve_covered_reconcile(
    record: &BloomRecord,
    member: &Membership,
    cursor: &StageProgress,
    bloom: BloomId,
    workpiece: &WorkpieceId,
    evidence: &Evidence,
) -> Option<Decisions> {
    let state = record.coordination.as_deref()?;
    let authored = cursor.candidate?;
    let claim = state.claims.get(workpiece.0.as_str())?;
    if claim.member.scope_revision != member.scope_revision || authored.tree != claim.member.candidate.tree {
        return None;
    }
    let carried =
        carried_head_pin(&state.integration.head, workpiece, claim.member.scope_revision, claim.member.candidate.tree)?;
    let progress = StageProgress {
        stage: StageId::Verify,
        attempts: 1,
        candidate: Some(carried.candidate),
        repair_rolls: cursor.repair_rolls,
        seen_verify_failures: cursor.seen_verify_failures,
        fold_checkpoint: None,
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
    };
    Some(Decisions {
        outcome: Outcome::AttemptAdvanced {
            bloom,
            workpiece: workpiece.clone(),
            from: StageId::Reconcile,
            to: StageId::Verify,
        },
        effects: alloc::vec![
            Decision::RecordEvidence { bloom, evidence: evidence.clone() },
            Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress },
        ],
    })
}

/// Park a construct that concluded without a candidate: record the evidence,
/// leave the cursor (and therefore `attempts` and `repair_rolls`) untouched,
/// and emit no dispatch. Distinct from [`wedged`]: a wedge spent the budget
/// and a grant buys more attempts; a park refused before it began and a
/// grant would buy another lap of the same refusal.
fn park_declined_construct(
    bloom: BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    evidence: &Evidence,
    effects: Vec<Decision>,
) -> Decisions {
    Decisions {
        outcome: Outcome::AttemptParked { bloom, workpiece: workpiece.clone(), stage, reason: evidence.detail },
        effects,
    }
}

/// Dispatch targets for a member-line move, or the folded checkpoint when
/// the member is reconciling a collision (ADR-0189).
///
/// Checkout precedence, when the member is not reconciling: a captured
/// candidate, else the newest construct checkpoint, else the sealed (or
/// spliced) base. A candidate outranks a checkpoint so finished work beats
/// partial work; a checkpoint never becomes the evidence-binding subject
/// (#4994).
///
/// A Construct dispatch always binds the scope revision, never a candidate
/// tree: the held candidate is the checkout the lane starts from, not the
/// subject the evidence binds to (#5968). Every other stage binds the
/// candidate tree once one exists.
///
/// The second return is the member's clean or spliced `base` only when the
/// checkout is that construct checkpoint — the provenance [`move_effects_with_checkpoint`]
/// stamps onto a Construct `diff_base`. Fold checkpoints, finished
/// candidates, and a cold or spliced-base start return `None`.
pub(super) fn reconcile_or_line_targets(
    stage: StageId,
    scope_revision: Digest,
    base: Digest,
    candidate: Option<CandidateRef>,
    fold_checkpoint: Option<Digest>,
    member_checkpoint: Option<CandidateRef>,
) -> (DispatchTargets, Option<Digest>) {
    if stage == StageId::Construct {
        if let Some(checkpoint) = fold_checkpoint {
            return (DispatchTargets { subject: scope_revision, checkout: checkpoint }, None);
        }
        return candidate.map_or_else(
            || {
                member_checkpoint.map_or(
                    (DispatchTargets { subject: scope_revision, checkout: base }, None),
                    |checkpoint| {
                        (DispatchTargets { subject: scope_revision, checkout: checkpoint.checkout }, Some(base))
                    },
                )
            },
            |current| (DispatchTargets { subject: scope_revision, checkout: current.checkout }, None),
        );
    }
    if let Some(checkpoint) = fold_checkpoint {
        return (
            DispatchTargets { subject: candidate.map_or(scope_revision, |current| current.tree), checkout: checkpoint },
            None,
        );
    }
    candidate.map_or_else(
        || {
            member_checkpoint
                .map_or((DispatchTargets { subject: scope_revision, checkout: base }, None), |checkpoint| {
                    (DispatchTargets { subject: scope_revision, checkout: checkpoint.checkout }, Some(base))
                })
        },
        |current| (DispatchTargets { subject: current.tree, checkout: current.checkout }, None),
    )
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

#[cfg(test)]
mod tests {
    use crate::testing::{step as testing_step, with_compiled_manifest};

    use super::super::coordination::initialized_effects;
    use super::*;
    use crate::ids::IdempotencyKey;
    use crate::reduce::{Event, Fact, GrantAttemptsError, Outcome};
    use crate::values::{
        BloomDraft, BloomSpec, ContextualResolutionClaim, CoordinationPolicy, EvidenceKind, MemberPin, Membership,
        OperatorHold, ResolutionProof, VerificationMode, VerifyProof,
    };

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn membership(name: &str, revision: u8) -> Membership {
        let mut member = Membership {
            workpiece: WorkpieceId(name.into()),
            scope_revision: digest(revision),
            configs: ConfigRegistry::default(),
            approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
        };
        member.approval.subject = member.subject();
        member
    }

    fn evidence() -> Evidence {
        Evidence { subject: digest(1), kind: EvidenceKind::VerificationResult, detail: digest(70) }
    }

    fn event(key: &str, fact: Fact) -> Event {
        Event { idempotency_key: IdempotencyKey(key.into()), fact }
    }

    fn step(snapshot: &Snapshot, event: &Event) -> (Snapshot, Decisions) {
        testing_step(snapshot, event)
    }

    fn sealed() -> (Snapshot, BloomId) {
        let spec = with_compiled_manifest(BloomDraft {
            proposals: vec![membership("wp", 10)],
            base: digest(0),
            ..BloomDraft::default()
        })
        .seal();
        let bloom = spec.id();
        let (snapshot, _) =
            step(&Snapshot::new(digest(0)).with_green_base(digest(0)), &event("seal", Fact::Seal(spec)));
        (snapshot, bloom)
    }

    fn fail_construct(bloom: BloomId, key: &str, captured: Option<CandidateRef>) -> Event {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: WorkpieceId("wp".into()),
                stage: StageId::Construct,
                passed: false,
                evidence: evidence(),
                candidate: captured,
            },
        )
    }

    fn decline_construct(bloom: BloomId, key: &str, reason: Digest) -> Event {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: WorkpieceId("wp".into()),
                stage: StageId::Construct,
                passed: false,
                evidence: Evidence { subject: digest(1), kind: EvidenceKind::ConstructDeclined, detail: reason },
                candidate: None,
            },
        )
    }

    fn pass_construct(bloom: BloomId, key: &str, captured: CandidateRef) -> Event {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: WorkpieceId("wp".into()),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence {
                    subject: captured.tree,
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(80),
                },
                candidate: Some(captured),
            },
        )
    }

    fn construct_dispatch(decisions: &Decisions) -> &Transformation {
        decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::DispatchAttempt { transformation, .. } => Some(transformation),
                _ => None,
            })
            .expect("expected a member-stage dispatch")
    }

    // Tripwire: checkout resolution is candidate over checkpoint over sealed
    // base. The plausible bug is any transposition — seeding from a checkpoint
    // when a finished candidate exists, or ignoring a checkpoint and paying
    // for a cold tree (#4994).
    #[test]
    fn checkout_prefers_candidate_over_checkpoint_over_sealed_base() {
        let base = digest(0);
        let scope = digest(10);
        let checkpoint = CandidateRef { tree: digest(21), checkout: digest(22) };
        let candidate = CandidateRef { tree: digest(31), checkout: digest(32) };

        let (cold, cold_mark) = reconcile_or_line_targets(StageId::Verify, scope, base, None, None, None);
        assert_eq!(cold.checkout, base, "neither a candidate nor a checkpoint checks out the sealed base");
        assert_eq!(cold.subject, scope);
        assert_eq!(cold_mark, None, "a cold or spliced-base start is unmarked");

        let (seeded, seeded_mark) =
            reconcile_or_line_targets(StageId::Verify, scope, base, None, None, Some(checkpoint));
        assert_eq!(seeded.checkout, checkpoint.checkout, "a checkpoint without a candidate seeds the checkout");
        assert_eq!(seeded.subject, scope, "a checkpoint is not a finished candidate: evidence still binds the scope");
        assert_eq!(seeded_mark, Some(base), "only the checkpoint case carries the member's base as provenance");

        let (finished, finished_mark) =
            reconcile_or_line_targets(StageId::Verify, scope, base, Some(candidate), None, Some(checkpoint));
        assert_eq!(finished.checkout, candidate.checkout, "a candidate outranks a checkpoint");
        assert_eq!(finished.subject, candidate.tree);
        assert_eq!(finished_mark, None, "a finished candidate is not a construct checkpoint");

        let (folded, folded_mark) =
            reconcile_or_line_targets(StageId::Reconcile, scope, base, None, Some(digest(40)), Some(checkpoint));
        assert_eq!(folded.checkout, digest(40), "a fold checkpoint outranks the construct checkpoint");
        assert_eq!(folded_mark, None, "a Reconcile fold checkout is not Construct provenance");

        // Tripwire (#5968): a Construct with a held candidate still binds the
        // scope revision — the candidate is the checkout, never the subject.
        let (held, held_mark) =
            reconcile_or_line_targets(StageId::Construct, scope, base, Some(candidate), None, Some(checkpoint));
        assert_eq!(held.checkout, candidate.checkout, "a held candidate is still the checkout the lane starts from");
        assert_eq!(held.subject, scope, "a Construct binds the scope revision even with a held candidate");
        assert_eq!(held_mark, None, "a held candidate is not a construct checkpoint");
    }

    // The plausible bug: a first construct is seeded from an empty checkpoint
    // slot and leaves the sealed base. Catches treating absence as a digest.
    #[test]
    fn a_fresh_construct_checks_out_the_sealed_base() {
        let spec = with_compiled_manifest(BloomDraft {
            proposals: vec![membership("wp", 10)],
            base: digest(0),
            ..BloomDraft::default()
        })
        .seal();
        let bloom = spec.id();
        let (after, decided) =
            step(&Snapshot::new(digest(0)).with_green_base(digest(0)), &event("seal", Fact::Seal(spec)));
        assert_eq!(
            construct_dispatch(&decided).checkout,
            digest(0),
            "a member with neither a candidate nor a checkpoint checks out the sealed base",
        );
        assert_eq!(
            construct_dispatch(&decided).diff_base,
            None,
            "a cold Construct must not carry the checkpoint provenance marker",
        );
        assert_eq!(after.member_checkpoint(&bloom, &WorkpieceId("wp".into())), None);
    }

    // The plausible bug: a failing construct's capture is adopted onto the
    // cursor, so a later pass would treat the checkpoint as a finished
    // candidate — or the retry still checks out the sealed base and the
    // checkpoint is never the tree the next attempt starts from.
    #[test]
    fn a_failing_construct_seeds_the_retry_from_its_checkpoint() {
        let (snapshot, bloom) = sealed();
        let checkpoint = CandidateRef { tree: digest(21), checkout: digest(22) };
        let (after, decided) = step(&snapshot, &fail_construct(bloom, "c-die", Some(checkpoint)));

        assert!(matches!(decided.outcome, Outcome::AttemptRetried { stage: StageId::Construct, attempt: 2, .. }));
        assert_eq!(
            after.member_checkpoint(&bloom, &WorkpieceId("wp".into())),
            Some(checkpoint),
            "the snapshot holds the newest construct checkpoint for #4994",
        );
        let Some(progress) = after.blooms.get(&bloom).and_then(|record| record.progress.get(&WorkpieceId("wp".into())))
        else {
            panic!("the sealed member has a construct cursor");
        };
        assert_eq!(progress.stage, StageId::Construct, "a dead attempt does not advance the cursor");
        assert_eq!(progress.candidate, None, "the cursor is not the checkpoint's home");
        match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
            Some(Decision::DispatchAttempt { transformation, candidate, .. }) => {
                assert_eq!(transformation.checkout, checkpoint.checkout, "the retry checks out the checkpoint");
                assert_eq!(*candidate, None, "the retry still binds the scope revision, not the checkpoint");
                assert_eq!(
                    transformation.diff_base,
                    Some(digest(0)),
                    "a seeded Construct names the member's base so the host can emit --seeded",
                );
            }
            other => panic!("expected a Construct retry, got {other:?}"),
        }
    }

    // The plausible bug: a kill plants a checkpoint, the retry still starts
    // cold, and a later pass's capture cannot prove the journaled checkout
    // was the checkpoint the death left (#4994 acceptance 5).
    #[test]
    fn a_killed_construct_seeds_the_retry_and_a_pass_then_captures() {
        let (snapshot, bloom) = sealed();
        let checkpoint = CandidateRef { tree: digest(21), checkout: digest(22) };
        let (after_kill, retried) = step(&snapshot, &fail_construct(bloom, "c-die", Some(checkpoint)));
        assert_eq!(
            construct_dispatch(&retried).checkout,
            checkpoint.checkout,
            "the resume-seeded retry journals the checkpoint as its checkout",
        );

        let captured = CandidateRef { tree: digest(31), checkout: digest(32) };
        let (after_pass, advanced) = step(&after_kill, &pass_construct(bloom, "c-pass", captured));
        assert!(matches!(
            advanced.outcome,
            Outcome::AttemptAdvanced { from: StageId::Construct, to: StageId::Verify, .. }
        ));
        assert_eq!(
            construct_dispatch(&advanced).checkout,
            captured.checkout,
            "a passing construct outranks the checkpoint it started from",
        );
        let Some(progress) =
            after_pass.blooms.get(&bloom).and_then(|record| record.progress.get(&WorkpieceId("wp".into())))
        else {
            panic!("the sealed member has a verify cursor");
        };
        assert_eq!(progress.candidate, Some(captured), "the pass adopts the capture, not the checkpoint");
    }

    // The plausible bug: grant re-aims from the cursor only and forgets the
    // snapshot checkpoint, so a wedged construct resumes cold.
    #[test]
    fn a_grant_after_a_wedged_construct_seeds_from_the_checkpoint() {
        let (snapshot, bloom) = sealed();
        let first = CandidateRef { tree: digest(21), checkout: digest(22) };
        let (snapshot, _) = step(&snapshot, &fail_construct(bloom, "c-die-1", Some(first)));
        let second = CandidateRef { tree: digest(31), checkout: digest(32) };
        let (snapshot, wedged) = step(&snapshot, &fail_construct(bloom, "c-die-2", Some(second)));
        assert!(matches!(wedged.outcome, Outcome::AttemptWedged { stage: StageId::Construct, .. }));

        let (_, granted) = step(
            &snapshot,
            &event(
                "grant",
                Fact::GrantAttempts {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Construct,
                    attempts: 1,
                },
            ),
        );
        assert!(matches!(granted.outcome, Outcome::AttemptsGranted { resumes_at: StageId::Construct, .. }));
        assert_eq!(
            construct_dispatch(&granted).checkout,
            second.checkout,
            "a grant resumes from the newest checkpoint, not the sealed base",
        );
        assert_eq!(
            construct_dispatch(&granted).diff_base,
            Some(digest(0)),
            "a granted Construct retry keeps the checkpoint provenance marker",
        );
    }

    // The plausible bug: hold/release re-aims from the cursor and forgets
    // the checkpoint marker, so a deferred Construct retry resumes as a
    // clean start and the prompt never names the untrusted tree.
    #[test]
    fn a_release_rederives_the_checkpoint_marker_on_a_deferred_construct_retry() {
        let (snapshot, bloom) = sealed();
        let hold = OperatorHold { reason: "stop".into(), operator: "op".into() };
        let (after_hold, _) = step(&snapshot, &event("hold", Fact::OperatorHold { bloom, hold: hold.clone() }));
        let checkpoint = CandidateRef { tree: digest(21), checkout: digest(22) };
        let (deferred, retried) = step(&after_hold, &fail_construct(bloom, "c-die", Some(checkpoint)));
        assert!(matches!(retried.outcome, Outcome::AttemptRetried { stage: StageId::Construct, .. }));
        assert!(
            retried.effects.iter().any(|effect| matches!(effect, Decision::DeferDispatch { .. })),
            "the hold swallows the retry work order",
        );

        let (_, released) = step(&deferred, &event("release", Fact::OperatorRelease { bloom, release: hold }));
        assert!(matches!(released.outcome, Outcome::BloomReleased { .. }));
        assert_eq!(
            construct_dispatch(&released).checkout,
            checkpoint.checkout,
            "the release checks out the checkpoint the death left",
        );
        assert_eq!(
            construct_dispatch(&released).diff_base,
            Some(digest(0)),
            "the release re-derives the same Construct checkpoint marker the grant path stamps",
        );
    }

    // The plausible bug: a second death overwrites the cursor but not the
    // snapshot slot, so #4994 would resume from the first partial tree.
    #[test]
    fn a_later_failing_construct_overwrites_the_checkpoint() {
        let (snapshot, bloom) = sealed();
        let first = CandidateRef { tree: digest(21), checkout: digest(22) };
        let (snapshot, _) = step(&snapshot, &fail_construct(bloom, "c-die-1", Some(first)));
        let second = CandidateRef { tree: digest(31), checkout: digest(32) };
        let (after, decided) = step(&snapshot, &fail_construct(bloom, "c-die-2", Some(second)));

        assert!(matches!(decided.outcome, Outcome::AttemptWedged { stage: StageId::Construct, .. }));
        assert_eq!(
            after.member_checkpoint(&bloom, &WorkpieceId("wp".into())),
            Some(second),
            "newest wins, including the death that exhausts the budget",
        );
        let Some(progress) = after.blooms.get(&bloom).and_then(|record| record.progress.get(&WorkpieceId("wp".into())))
        else {
            panic!("the sealed member has a construct cursor");
        };
        assert_eq!(progress.candidate, None);
    }

    // The plausible bug: a clean death plants an empty checkpoint the retry
    // would then treat as work.
    #[test]
    fn a_failing_construct_without_a_capture_records_no_checkpoint() {
        let (snapshot, bloom) = sealed();
        let (after, decided) = step(&snapshot, &fail_construct(bloom, "c-empty", None));
        assert_eq!(after.member_checkpoint(&bloom, &WorkpieceId("wp".into())), None);
        assert_eq!(
            construct_dispatch(&decided).checkout,
            digest(0),
            "a clean death does not invent a checkout to seed from",
        );
    }

    // The plausible bug: a construct that concluded without a candidate is
    // treated as a dead attempt, so attempts increment and the member wedges
    // once the sealed budget is spent — granting more attempts then replays
    // the same refusal (#5292).
    #[test]
    fn a_declined_construct_parks_without_spending_an_attempt() {
        let (snapshot, bloom) = sealed();
        let workpiece = WorkpieceId("wp".into());
        let before = snapshot.blooms[&bloom].progress[&workpiece];
        let reason = digest(91);
        let (after, decided) = step(&snapshot, &decline_construct(bloom, "c-decline", reason));

        assert!(
            matches!(
                decided.outcome,
                Outcome::AttemptParked { stage: StageId::Construct, reason: got, .. } if got == reason
            ),
            "a declined construct parks naming the lane's evidence: {decided:?}",
        );
        assert!(
            !decided.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
            "a park must not dispatch another construct lap",
        );
        assert!(
            !decided.effects.iter().any(|effect| matches!(effect, Decision::AdvanceStage { .. })),
            "a park must not move the cursor, or attempts would change",
        );
        assert!(
            !decided.effects.iter().any(|effect| matches!(effect, Decision::RecordWedge { .. })),
            "a park is not a wedge",
        );
        let after_cursor = after.blooms[&bloom].progress[&workpiece];
        assert_eq!(after_cursor.attempts, before.attempts, "parking spends no attempt");
        assert_eq!(after_cursor.repair_rolls, before.repair_rolls, "parking spends no repair roll");
        assert_eq!(after_cursor.stage, StageId::Construct);
        assert_eq!(
            after.member_park(&bloom, &workpiece).map(|park| park.evidence),
            Some(reason),
            "the snapshot holds the park so the served view can name the member",
        );
        assert!(!after.blooms[&bloom].wedged.contains_key(&workpiece), "a parked member is not wedged");
    }

    /// A sealed bloom with coordination state whose head carries `wp`'s claim,
    /// and whose cursor sits at Reconcile on `cursor_candidate`.
    fn reconcile_on_carried_claim(cursor_candidate: CandidateRef) -> (Snapshot, BloomId, CandidateRef) {
        let (snapshot, bloom) = sealed();
        let mut record = snapshot.blooms.get(&bloom).expect("sealed bloom").clone();
        let mut state = initialized_effects(
            &Snapshot::new(record.spec.base()),
            &record.spec,
            &record.stage_catalog,
            &record.pipeline_manifest,
            Some(CoordinationPolicy {
                verification: VerificationMode::Contextual,
                eager_integration: true,
                max_run_members: 4,
                max_serial_requests: 4,
                max_attribution_probes: 3,
                movement_budget: 2,
                reservation_millis: 1_000,
                coalesce_millis: None,
                host_class: String::from("test-host"),
            }),
        )
        .expect("valid policy")
        .into_iter()
        .find_map(|effect| match effect {
            Decision::RecordCoordinationState { state: Some(state), .. } => Some(*state),
            _ => None,
        })
        .expect("coordination state");
        let carried = CandidateRef { tree: digest(40), checkout: digest(41) };
        let pin = MemberPin { workpiece: WorkpieceId("wp".into()), scope_revision: digest(10), candidate: carried };
        state.claims.insert(
            String::from("wp"),
            ContextualResolutionClaim {
                member: pin.clone(),
                proof: ResolutionProof::Standalone(VerifyProof {
                    stage: StageId::Verify,
                    gate_set: Digest::default(),
                    evidence: Evidence {
                        subject: carried.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(70),
                    },
                }),
            },
        );
        state.integration.head.coverage = alloc::vec![pin];
        state.integration.head.generation = state.integration.generation.digest();
        record.coordination = Some(Box::new(state));
        record.progress.insert(
            WorkpieceId("wp".into()),
            StageProgress {
                stage: StageId::Reconcile,
                attempts: 1,
                candidate: Some(cursor_candidate),
                repair_rolls: 0,
                seen_verify_failures: VerifyFailureSet::EMPTY,
                fold_checkpoint: Some(digest(61)),
                fold_conflict_evidence: Some(digest(62)),
                reconcile_assembles_base: false,
            },
        );
        let mut snapshot = snapshot;
        snapshot.blooms.insert(bloom, record);
        (snapshot, bloom, carried)
    }

    fn decline_reconcile(bloom: BloomId, key: &str) -> Event {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: WorkpieceId("wp".into()),
                stage: StageId::Reconcile,
                passed: false,
                evidence: Evidence { subject: digest(1), kind: EvidenceKind::ConstructDeclined, detail: digest(91) },
                candidate: None,
            },
        )
    }

    // The plausible bug: a Reconcile lane that declines because the head
    // already carries its subject parks the member at Reconcile while its
    // claim stands, and the console reads a stuck bloom (#5966).
    #[test]
    fn a_reconcile_decline_on_head_carried_work_resolves_current() {
        let carried = CandidateRef { tree: digest(40), checkout: digest(41) };
        let (snapshot, bloom, _) = reconcile_on_carried_claim(carried);
        let workpiece = WorkpieceId("wp".into());
        let (after, decided) = step(&snapshot, &decline_reconcile(bloom, "r-decline"));

        assert!(
            matches!(decided.outcome, Outcome::AttemptAdvanced { from: StageId::Reconcile, to: StageId::Verify, .. }),
            "the decline resolves as current instead of parking: {decided:?}",
        );
        let advanced = decided
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::AdvanceStage { progress, .. } => Some(*progress),
                _ => None,
            })
            .expect("the cursor leaves Reconcile");
        assert_eq!(advanced.stage, StageId::Verify);
        assert_eq!(advanced.candidate, Some(carried));
        assert!(
            !decided.effects.iter().any(|effect| {
                matches!(
                    effect,
                    Decision::DispatchAttempt { .. }
                        | Decision::DispatchContextualAttempt { .. }
                        | Decision::DispatchCandidatePreparation { .. }
                        | Decision::QueueMemberVerification { .. }
                )
            }),
            "resolving as current dispatches nothing — the head's work is already proven",
        );
        let cursor = after.blooms[&bloom].progress[&workpiece];
        assert_eq!(cursor.stage, StageId::Verify);
        assert!(after.member_park(&bloom, &workpiece).is_none(), "no park is recorded");
    }

    // The plausible bug: the resolve-as-current arm fires on any Reconcile
    // decline under coordination, silently dropping work the head never
    // carried.
    #[test]
    fn a_reconcile_decline_on_work_the_head_does_not_carry_still_parks() {
        let other = CandidateRef { tree: digest(42), checkout: digest(43) };
        let (mut snapshot, bloom, _) = reconcile_on_carried_claim(other);
        let workpiece = WorkpieceId("wp".into());
        let record = snapshot.blooms.get_mut(&bloom).expect("sealed bloom");
        let state = record.coordination.as_mut().expect("coordination state");
        state.claims.clear();
        state.integration.head.coverage.clear();
        let reason = digest(91);
        let (after, decided) = step(&snapshot, &decline_reconcile(bloom, "r-decline"));

        assert!(
            matches!(
                decided.outcome,
                Outcome::AttemptParked { stage: StageId::Reconcile, reason: got, .. } if got == reason
            ),
            "an uncovered decline still parks: {decided:?}",
        );
        assert_eq!(after.member_park(&bloom, &workpiece).map(|park| park.stage), Some(StageId::Reconcile),);
    }

    // The plausible bug: the resolve-as-current arm matches the claim without
    // checking the lap was for it, so a decline for a newer version resolves
    // onto the older carried pin and strands the newer work.
    #[test]
    fn a_reconcile_decline_for_a_newer_version_than_the_claim_still_parks() {
        let other = CandidateRef { tree: digest(42), checkout: digest(43) };
        let (snapshot, bloom, _) = reconcile_on_carried_claim(other);
        let workpiece = WorkpieceId("wp".into());
        let (after, decided) = step(&snapshot, &decline_reconcile(bloom, "r-decline"));

        assert!(
            matches!(decided.outcome, Outcome::AttemptParked { stage: StageId::Reconcile, .. }),
            "a decline for another version still parks: {decided:?}",
        );
        assert!(after.member_park(&bloom, &workpiece).is_some(), "the park stays visible");
    }

    // The plausible bug: a refused grant aimed at a parked member still
    // erases the park, so the member has no excuse, no wedge, and no worker,
    // and a second grant cannot re-plant it.
    #[test]
    fn a_refused_grant_leaves_the_park_standing() {
        let (snapshot, bloom) = sealed();
        let workpiece = WorkpieceId("wp".into());
        let (parked, _) = step(&snapshot, &decline_construct(bloom, "c-decline", digest(91)));
        assert!(parked.member_park(&bloom, &workpiece).is_some(), "the park is what a grant must not erase");

        let (after, decided) = step(
            &parked,
            &event(
                "grant",
                Fact::GrantAttempts { bloom, workpiece: workpiece.clone(), stage: StageId::Construct, attempts: 1 },
            ),
        );
        assert!(
            matches!(
                decided.outcome,
                Outcome::GrantAttemptsRejected(GrantAttemptsError::NotWedged(ref wp)) if *wp == workpiece
            ),
            "a parked member is not wedged, so the grant is refused: {decided:?}",
        );
        assert!(
            after.member_park(&bloom, &workpiece).is_some(),
            "a fact the reducer rejected changes nothing in the fold",
        );
    }

    // The plausible bug: gating the park clear on the accepted outcome never
    // opens, so a park outlives the integration that made it false.
    #[test]
    fn an_integration_still_clears_the_park() {
        let (snapshot, bloom) = sealed();
        let workpiece = WorkpieceId("wp".into());
        let (parked, _) = step(&snapshot, &decline_construct(bloom, "c-decline", digest(91)));
        assert!(parked.member_park(&bloom, &workpiece).is_some(), "the park is what this integrate has to lift");

        let (after, decided) =
            step(&parked, &event("integrate", Fact::Integrate { bloom, claim: claim("wp", 10, 51) }));
        assert!(
            matches!(decided.outcome, Outcome::Integrated { .. }),
            "the gate opens on the accepted integration: {decided:?}",
        );
        assert_eq!(after.member_park(&bloom, &workpiece), None, "an integration redeems the park");
    }

    // The plausible bug: inferring a park from `candidate: None` would convert
    // a genuine crash retry into a park and strand a member that would have
    // succeeded on a second attempt.
    #[test]
    fn a_construct_that_dies_without_a_terminal_record_still_retries() {
        let (snapshot, bloom) = sealed();
        let (after, decided) = step(&snapshot, &fail_construct(bloom, "c-die", None));
        assert!(matches!(decided.outcome, Outcome::AttemptRetried { stage: StageId::Construct, attempt: 2, .. }));
        assert_eq!(after.blooms[&bloom].progress[&WorkpieceId("wp".into())].attempts, 2);
        assert_eq!(after.member_park(&bloom, &WorkpieceId("wp".into())), None);
    }

    // The plausible bug: a construct host fault shares "no candidate" with a
    // declined construct, so the machinery retry is reclassified as a park and
    // a recoverable outage becomes a scope problem.
    #[test]
    fn a_construct_host_fault_still_retries_rather_than_parking() {
        let (snapshot, bloom) = sealed();
        let (_, decided) = step(
            &snapshot,
            &event(
                "fault",
                Fact::MemberExecutorFault {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Construct,
                    evidence: Evidence { subject: digest(10), kind: EvidenceKind::ExecutorFault, detail: digest(60) },
                },
            ),
        );
        assert!(
            matches!(decided.outcome, Outcome::MachineryRetried { stage: StageId::Construct, .. }),
            "a construct host fault stays on the machinery axis: {decided:?}",
        );
        assert!(
            !matches!(decided.outcome, Outcome::AttemptParked { .. }),
            "a host fault must not be reclassified as a construct park",
        );
    }

    fn claim(name: &str, revision: u8, candidate: u8) -> ResolutionClaim {
        let candidate = digest(candidate);
        ResolutionClaim {
            workpiece: WorkpieceId(name.into()),
            scope_revision: digest(revision),
            candidate,
            evidence: Evidence { subject: candidate, kind: EvidenceKind::ResolutionClaim, detail: digest(201) },
        }
    }

    fn conflict_evidence(checkpoint: u8) -> Evidence {
        Evidence { subject: digest(checkpoint), kind: EvidenceKind::FoldConflict, detail: digest(90) }
    }

    fn two_member_spec(base: u8) -> BloomSpec {
        with_compiled_manifest(BloomDraft {
            proposals: vec![membership("alpha", 10), membership("beta", 11)],
            base: digest(base),
            ..BloomDraft::default()
        })
        .seal()
    }

    fn construct_and_integrate(mut snapshot: Snapshot, bloom: BloomId) -> Snapshot {
        for (name, revision, tree, checkout) in [("alpha", 10, 20, 22), ("beta", 11, 21, 23)] {
            snapshot = step(
                &snapshot,
                &event(
                    &format!("construct-{name}"),
                    Fact::AttemptCompleted {
                        bloom,
                        workpiece: WorkpieceId(name.into()),
                        stage: StageId::Construct,
                        passed: true,
                        evidence: Evidence {
                            subject: digest(tree),
                            kind: EvidenceKind::VerificationResult,
                            detail: digest(80),
                        },
                        candidate: Some(CandidateRef { tree: digest(tree), checkout: digest(checkout) }),
                    },
                ),
            )
            .0;
            snapshot = step(
                &snapshot,
                &event(&format!("integrate-{name}"), Fact::Integrate { bloom, claim: claim(name, revision, tree) }),
            )
            .0;
        }
        snapshot
    }

    fn fold_beta(snapshot: &Snapshot, bloom: BloomId, key: &str, checkpoint: u8, head: u8) -> Snapshot {
        step(
            snapshot,
            &event(
                key,
                Fact::FoldConflict {
                    bloom,
                    workpiece: WorkpieceId("beta".into()),
                    checkpoint: digest(checkpoint),
                    head: digest(head),
                    evidence: conflict_evidence(checkpoint),
                },
            ),
        )
        .0
    }

    fn pass_reconcile(
        snapshot: &Snapshot,
        bloom: BloomId,
        workpiece: &str,
        key: &str,
        captured: CandidateRef,
    ) -> Decisions {
        step(
            snapshot,
            &event(
                key,
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: WorkpieceId(workpiece.into()),
                    stage: StageId::Reconcile,
                    passed: true,
                    evidence: evidence(),
                    candidate: Some(captured),
                },
            ),
        )
        .1
    }

    fn dispatch_stage_and_subject(decisions: &Decisions) -> (StageId, Digest) {
        decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::DispatchAttempt { stage, transformation, .. } => Some((*stage, transformation.inputs[0])),
                _ => None,
            })
            .expect("expected a member-stage dispatch")
    }

    fn inherited_claim_successor() -> (Snapshot, BloomId) {
        let spec = two_member_spec(0);
        let predecessor = spec.id();
        let (snapshot, _) =
            step(&Snapshot::new(digest(0)).with_green_base(digest(0)), &event("seal", Fact::Seal(spec)));
        let snapshot = construct_and_integrate(snapshot, predecessor);
        let snapshot = step(&snapshot, &event("observe-2", Fact::ObserveMainline { head: digest(2) }))
            .0
            .with_green_base(digest(2));
        let successor_spec = two_member_spec(2);
        let successor = successor_spec.id();
        let (snapshot, decided) =
            step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));
        assert!(matches!(decided.outcome, Outcome::Superseded { .. }), "the successor seals: {decided:?}");
        (snapshot, successor)
    }

    // The plausible bug: an inherited claim has no cursor, so after FoldConflict
    // revokes it both halves of the old inference read as a base assembly and
    // the next dispatch is Construct against the scope revision.
    #[test]
    fn an_inherited_claim_folded_with_a_conflict_reconciles_back_to_verify() {
        let (snapshot, bloom) = inherited_claim_successor();
        let snapshot = fold_beta(&snapshot, bloom, "fold-conflict-beta", 30, 31);
        let captured = CandidateRef { tree: digest(41), checkout: digest(42) };
        let decided = pass_reconcile(&snapshot, bloom, "beta", "reconcile-pass", captured);

        match decided.outcome {
            Outcome::AttemptAdvanced { from, to, .. } => {
                assert_eq!(from, StageId::Reconcile);
                assert_eq!(to, StageId::Verify);
            }
            other => panic!("expected AttemptAdvanced onto Verify, got {other:?}"),
        }
        assert_eq!(
            dispatch_stage_and_subject(&decided),
            (StageId::Verify, captured.tree),
            "Verify re-targets from the reconciled tree, not the scope revision",
        );
    }

    // The plausible bug: recording the assembly bit for every FoldConflict,
    // including a member that resolved in this bloom and still has a Verify
    // cursor, would send that member back to Construct.
    #[test]
    fn a_member_resolved_in_this_bloom_still_reconciles_back_to_verify() {
        let spec = two_member_spec(0);
        let bloom = spec.id();
        let (snapshot, _) =
            step(&Snapshot::new(digest(0)).with_green_base(digest(0)), &event("seal", Fact::Seal(spec)));
        let snapshot = construct_and_integrate(snapshot, bloom);
        let snapshot = fold_beta(&snapshot, bloom, "fold-conflict-beta", 30, 31);
        let captured = CandidateRef { tree: digest(41), checkout: digest(42) };
        let decided = pass_reconcile(&snapshot, bloom, "beta", "reconcile-pass", captured);

        match decided.outcome {
            Outcome::AttemptAdvanced { from, to, .. } => {
                assert_eq!(from, StageId::Reconcile);
                assert_eq!(to, StageId::Verify);
            }
            other => panic!("expected AttemptAdvanced onto Verify, got {other:?}"),
        }
        assert_eq!(dispatch_stage_and_subject(&decided), (StageId::Verify, captured.tree));
    }

    // The plausible bug: requiring a claim to route to Verify would send a
    // dependent's base-assembly Reconcile into Verify against a tree it never
    // authored (ADR-0196).
    #[test]
    fn a_dependents_base_assembly_still_returns_to_construct() {
        let (snapshot, bloom) = sealed();
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "splice-conflict",
                Fact::FoldConflict {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    checkpoint: digest(30),
                    head: digest(31),
                    evidence: conflict_evidence(30),
                },
            ),
        );
        let captured = CandidateRef { tree: digest(41), checkout: digest(42) };
        let decided = pass_reconcile(&snapshot, bloom, "wp", "reconcile-pass", captured);

        match decided.outcome {
            Outcome::AttemptAdvanced { from, to, .. } => {
                assert_eq!(from, StageId::Reconcile);
                assert_eq!(to, StageId::Construct);
            }
            other => panic!("expected AttemptAdvanced onto Construct, got {other:?}"),
        }
        assert_eq!(
            construct_dispatch(&decided).checkout,
            captured.checkout,
            "Construct checks out the assembled capture",
        );
    }

    // Tripwire (#5968): a Construct issued from a Reconcile completion on a
    // held candidate binds the scope revision, never the candidate tree. The
    // plausible bug is two builders disagreeing — the issue-time order
    // displaying the candidate while carrying the scope as inputs[0] — so a
    // real lane's evidence binds inputs[0] and intake refuses it.
    #[test]
    fn a_base_assembly_construct_binds_the_scope_revision() {
        let (snapshot, bloom) = sealed();
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "splice-conflict",
                Fact::FoldConflict {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    checkpoint: digest(30),
                    head: digest(31),
                    evidence: conflict_evidence(30),
                },
            ),
        );
        let captured = CandidateRef { tree: digest(41), checkout: digest(42) };
        let decided = pass_reconcile(&snapshot, bloom, "wp", "reconcile-pass", captured);

        let transformation = construct_dispatch(&decided);
        assert_eq!(
            transformation.inputs[0],
            digest(10),
            "a held candidate is the checkout, never the evidence-binding subject",
        );
        assert_eq!(transformation.checkout, captured.checkout, "Construct still starts from the assembled tree");
        match decided.effects.iter().find(|effect| matches!(effect, Decision::DispatchAttempt { .. })) {
            Some(Decision::DispatchAttempt { candidate, .. }) => {
                assert_eq!(*candidate, None, "a Construct displays the scope revision, never a held candidate");
            }
            other => panic!("expected a Construct dispatch, got {other:?}"),
        }
    }

    // The plausible bug: leaving `reconcile_assembles_base` set after the
    // assembly's Construct is dispatched would send a later genuine revocation
    // on the same member back to Construct.
    #[test]
    fn the_assembly_bit_does_not_outlive_its_reconcile() {
        let (snapshot, bloom) = sealed();
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "splice-conflict",
                Fact::FoldConflict {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    checkpoint: digest(30),
                    head: digest(31),
                    evidence: conflict_evidence(30),
                },
            ),
        );
        let assembled = CandidateRef { tree: digest(41), checkout: digest(42) };
        let (snapshot, advanced) = step(
            &snapshot,
            &event(
                "reconcile-pass",
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Reconcile,
                    passed: true,
                    evidence: evidence(),
                    candidate: Some(assembled),
                },
            ),
        );
        assert!(matches!(
            advanced.outcome,
            Outcome::AttemptAdvanced { from: StageId::Reconcile, to: StageId::Construct, .. }
        ));
        let progress = snapshot.blooms[&bloom].progress[&WorkpieceId("wp".into())];
        assert!(!progress.reconcile_assembles_base, "the bit is a property of one Reconcile lap");

        let work = CandidateRef { tree: digest(51), checkout: digest(52) };
        let (snapshot, _) = step(&snapshot, &pass_construct(bloom, "c-pass", work));
        let (snapshot, _) =
            step(&snapshot, &event("integrate-wp", Fact::Integrate { bloom, claim: claim("wp", 10, 51) }));
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "fold-after-claim",
                Fact::FoldConflict {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    checkpoint: digest(32),
                    head: digest(33),
                    evidence: conflict_evidence(32),
                },
            ),
        );
        let reconciled = CandidateRef { tree: digest(61), checkout: digest(62) };
        let decided = pass_reconcile(&snapshot, bloom, "wp", "reconcile-after-claim", reconciled);

        match decided.outcome {
            Outcome::AttemptAdvanced { from, to, .. } => {
                assert_eq!(from, StageId::Reconcile);
                assert_eq!(to, StageId::Verify);
            }
            other => panic!("expected the later revocation to rejoin Verify, got {other:?}"),
        }
    }

    // A member at Construct holding a candidate from an admitted Reconcile
    // completion accepts `retry --stage Verify`: the held capture moves onto
    // its verify without another construct lap.
    #[test]
    fn a_retry_naming_verify_is_accepted_when_the_member_holds_a_candidate() {
        let (snapshot, bloom) = sealed();
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "splice-conflict",
                Fact::FoldConflict {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    checkpoint: digest(30),
                    head: digest(31),
                    evidence: conflict_evidence(30),
                },
            ),
        );
        let assembled = CandidateRef { tree: digest(41), checkout: digest(42) };
        let (snapshot, advanced) = step(
            &snapshot,
            &event(
                "reconcile-pass",
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Reconcile,
                    passed: true,
                    evidence: evidence(),
                    candidate: Some(assembled),
                },
            ),
        );
        assert!(matches!(
            advanced.outcome,
            Outcome::AttemptAdvanced { from: StageId::Reconcile, to: StageId::Construct, .. }
        ));
        let progress = snapshot.blooms[&bloom].progress[&WorkpieceId("wp".into())];
        assert_eq!(progress.stage, StageId::Construct);
        assert_eq!(progress.candidate, Some(assembled));
        assert_eq!(progress.fold_checkpoint, Some(digest(31)), "the capture carries the head it reconciled onto");

        let (after, retried) = step(
            &snapshot,
            &event(
                "retry-verify",
                Fact::MemberExecutorFault {
                    bloom,
                    workpiece: WorkpieceId("wp".into()),
                    stage: StageId::Verify,
                    evidence: Evidence {
                        subject: assembled.tree,
                        kind: EvidenceKind::ExecutorFault,
                        detail: digest(71),
                    },
                },
            ),
        );
        let Outcome::MachineryRetried { stage, .. } = retried.outcome else {
            panic!("expected a Verify retry, got {:?}", retried.outcome);
        };
        assert_eq!(stage, StageId::Verify);
        let (stage, subject) = dispatch_stage_and_subject(&retried);
        assert_eq!(stage, StageId::Verify);
        assert_eq!(subject, assembled.tree);

        // Tripwire (#4952): the fold round outlives the stage. Dropping the
        // checkpoint on the way to Verify would leave the fold with no record
        // of what this capture was reconciled onto, and the next fold would
        // replay the conflict this reconcile already resolved.
        let moved = after.blooms[&bloom].progress[&WorkpieceId("wp".into())];
        assert_eq!(moved.stage, StageId::Verify);
        assert_eq!(moved.candidate, Some(assembled));
        assert_eq!(moved.fold_checkpoint, Some(digest(31)));
    }
}
