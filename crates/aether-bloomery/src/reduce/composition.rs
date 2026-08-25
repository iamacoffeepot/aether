//! The composition workpiece (ADR-0191): the synthetic subject whose candidate
//! is the weave of every member's candidate.
//!
//! A bloom's members construct in parallel against one sealed base, so the fold
//! is deferred — and until now the fold was a *phase* of the bloom rather than a
//! thing in its own right. It had a diff and an author (the reconcile lane
//! writes real code at every seam, ADR-0189) but no candidate slot, no budget,
//! no repair lap, and no channel to receive a finding. So a defect discovered in
//! the composed tree had no owner, and the only levers the reducer held were
//! member-shaped: it scattered the refusal onto the nearest owners and re-opened
//! them at the entry stage. That is how bloom `05b1f598` discarded four
//! finished, reviewed candidates on one aggregate refusal.
//!
//! Under ADR-0191 the composition is a workpiece. It takes the same maps a
//! member takes — a stage cursor in [`BloomRecord::progress`], a wedge in
//! [`BloomRecord::wedged`], a slot in the dispatch ledger — keyed by the
//! reserved [`WorkpieceId::COMPOSITION`]. A refusal of the composed tree
//! therefore has somewhere to go that is not a member: it files a finding on the
//! composition's own channel and dispatches the composition's `Refine`, the
//! **weave repair**, against the composed tree that was refused. Members are
//! immutable after review — no claim is revoked, no member is dispatched, and
//! a finding that is genuinely about member code is recorded as new work rather
//! than re-opening finished work.

use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate};
use super::{
    AttemptCompletedError, BloomRecord, BloomStatus, Decision, Decisions, FoldedIntegration, Outcome, Snapshot,
    StageProgress,
};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{CandidateRef, CompositionFinding, Evidence, VerifyFailureSet, Wedge};

/// One refusal of a bloom's composed tree — everything the weave repair needs to
/// be aimed, from whichever gate refused it.
///
/// The three gates that judge the composition ([`StageId::AggregateVerify`], its
/// [`StageId::AggregateReview`], and the landing gate at [`StageId::Land`])
/// differ only in `refused_at` and in which digests they carry, so they hand
/// this one value to [`reweave`] instead of each growing its own routing.
pub(super) struct Refusal<'a> {
    /// Which gate refused the composition.
    pub refused_at: StageId,
    /// The composed tree that was refused — the composition's current candidate
    /// and the subject its repair's evidence binds.
    pub tree: Digest,
    /// The commit carrying that tree — what the repair lane checks out.
    pub head: Digest,
    /// The refusing verdict.
    pub evidence: &'a Evidence,
    /// The members the verdict implicated, if it named any. Recorded on the
    /// finding and read by nothing else: naming a member files follow-up work,
    /// it never dispatches against one.
    pub implicated: &'a [WorkpieceId],
}

/// One composition's cursor, or `None` before its first repair.
fn cursor(record: &BloomRecord, composition: &WorkpieceId) -> Option<StageProgress> {
    record.progress.get(composition).copied()
}

/// The weave-repair attempt allowance: the sealed catalog's `Refine` budget, the
/// same one a member's repair loop spends. The composition repairs the way a
/// member repairs, so it is bounded the way a member is bounded.
fn repair_budget(record: &BloomRecord) -> u32 {
    record.stage_catalog.retry_budget_of(StageId::Refine).unwrap_or(1)
}

/// The composition's stage cursor after a move to `stage` on its `attempt`th
/// try, carrying `weave` as the candidate every later dispatch re-targets from.
pub(super) fn composition_progress(stage: StageId, attempt: u32, weave: CandidateRef) -> StageProgress {
    StageProgress {
        stage,
        attempts: attempt,
        candidate: Some(weave),
        // The composition takes no repair-roll or verify-failure history: those
        // count *repeated* terminal member-Verify verdicts (ADR-0178), and the
        // composition's own ceiling is the weave-repair budget below.
        repair_rolls: 0,
        seen_verify_failures: VerifyFailureSet::EMPTY,
        fold_checkpoint: None,
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
    }
}

/// The line the composition dispatches under: the bloom's own configuration and
/// sealed catalog, over the base every member's candidate was built onto.
///
/// Bloom-wide rather than member-layered, because the composition is nobody's
/// member — its subject is the whole weave, so a single member's registry has no
/// standing over it.
pub(super) fn composition_line(record: &BloomRecord) -> SealedLine<'_> {
    SealedLine {
        configs: record.spec.configs().clone(),
        catalog: &record.stage_catalog,
        base: record.spec.base(),
        held: record.operator_hold.is_some(),
        base_proven: record.base_proven,
    }
}

/// The finding one refusal of the composed tree files on the composition's
/// channel (ADR-0191 §4): what was judged, the verdict artifact that says so,
/// and whichever members the verdict named.
///
/// Named once because every path that refuses the composition reaches it — the
/// re-weave below, the three gate ceilings that park instead of repairing
/// (#4977), and the advisory-carrying pass that files an observation on its way
/// to the landing. A park is a refusal holding its evidence exactly as a
/// re-weave is; what differs is that the budget is spent. So it files the same
/// row, and the readers of the channel — the operator's adjudication door, the
/// study that counts refusals — see a ceiling refusal the way they see a
/// re-weave's, instead of the escalated refusals being the ones missing from
/// the count.
pub(super) fn finding_of(bloom: BloomId, tree: Digest, evidence: &Evidence, implicated: &[WorkpieceId]) -> Decision {
    Decision::RecordCompositionFinding {
        bloom,
        finding: CompositionFinding { subject: tree, detail: evidence.detail, implicated: implicated.to_vec() },
    }
}

/// Repair a refused composition **in the composition** (ADR-0191 §5).
///
/// The effects, in order: the finding on the composition's channel, then either
/// the weave repair's cursor move plus its dispatch, or — when the repair budget
/// is already spent — the composition's wedge. A refusal that arrives while a
/// repair of the same tree is already out files only the finding: the two gates
/// run concurrently, and one refused fold buys one repair lap.
///
/// What this deliberately does *not* emit is the whole point of ADR-0191: no
/// [`Decision::RevokeResolution`], and no [`Decision::DispatchAttempt`] naming a
/// member. A member that has passed its review is done. The held fold is left
/// standing too, because it is the composition's candidate under repair rather
/// than a stale artifact of someone else's work.
pub(super) fn reweave(record: &BloomRecord, bloom: &BloomId, refusal: &Refusal<'_>) -> Decisions {
    let composition = WorkpieceId::composition();
    let mut effects = alloc::vec![finding_of(*bloom, refusal.tree, refusal.evidence, refusal.implicated)];

    // The two composite gates judge one fold at the same time, so both can
    // refuse it. The first refusal already put a repair lane on this exact
    // tree; a second would double-spend the weave budget and set two lanes
    // writing one seam. So the second files its finding — the verdict is real
    // and belongs on the channel — and stops there.
    if cursor(record, &composition).is_some_and(|progress| {
        progress.stage == StageId::Refine && progress.candidate.is_some_and(|weave| weave.tree == refusal.tree)
    }) {
        return Decisions {
            outcome: Outcome::CompositionRepairInFlight { bloom: *bloom, refused_at: refusal.refused_at },
            effects,
        };
    }

    let spent = cursor(record, &composition)
        .filter(|progress| progress.stage == StageId::Refine)
        .map_or(0, |progress| progress.attempts);
    let attempt = spent + 1;
    if attempt > repair_budget(record) {
        effects.push(Decision::RecordWedge {
            bloom: *bloom,
            workpiece: composition,
            wedge: Wedge {
                stage: StageId::Refine,
                evidence: refusal.evidence.detail,
                repeated_verifiers: VerifyFailureSet::EMPTY,
            },
        });
        return Decisions {
            outcome: Outcome::CompositionWedged {
                bloom: *bloom,
                refused_at: refusal.refused_at,
                question: refusal.evidence.detail,
            },
            effects,
        };
    }

    let weave = CandidateRef { tree: refusal.tree, checkout: refusal.head };
    effects.extend(move_effects_with_candidate(
        *bloom,
        &composition,
        record.spec.base(),
        composition_progress(StageId::Refine, attempt, weave),
        DispatchTargets { subject: weave.tree, checkout: weave.checkout },
        Some(weave.tree),
        composition_line(record),
    ));

    Decisions {
        outcome: Outcome::CompositionRewoven { bloom: *bloom, refused_at: refusal.refused_at, attempt },
        effects,
    }
}

/// Put the member whose verdict minted `composition` back on its own Verify,
/// against the tree the repair produced (ADR-0210).
///
/// Empty when the snapshot holds no narrowing for this composition, when the
/// member has since left the membership, or when it is no longer sitting at the
/// Verify its verdict came from — each of those is a member that has already
/// moved, and re-entering it would overwrite a cursor somebody else set.
fn reverify_after_repair(
    snapshot: &Snapshot,
    record: &BloomRecord,
    bloom: &BloomId,
    composition: &WorkpieceId,
    repaired: CandidateRef,
) -> Vec<Decision> {
    let Some(narrowed) = snapshot.narrowed_compositions_of(bloom).find(|(id, _)| *id == composition).map(|(_, it)| it)
    else {
        return Vec::new();
    };
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == narrowed.verified) else {
        return Vec::new();
    };
    if record.progress.get(&narrowed.verified).is_none_or(|progress| progress.stage != StageId::Verify) {
        return Vec::new();
    }

    move_effects_with_candidate(
        *bloom,
        &narrowed.verified,
        member.scope_revision,
        composition_progress(StageId::Verify, 1, repaired),
        DispatchTargets { subject: repaired.tree, checkout: repaired.checkout },
        Some(repaired.tree),
        SealedLine::of(record, member),
    )
    .to_vec()
}

/// Reduce a weave-repair completion — a [`Fact::AttemptCompleted`](crate::Fact::AttemptCompleted)
/// naming the composition workpiece.
///
/// The composition's `Refine` is the only stage dispatched through this door: its
/// `Construct` is the fold (driven by [`Decision::DispatchIntegration`] and
/// completing at [`Fact::Resolve`](crate::Fact::Resolve)). The two instances then
/// diverge. The whole-bloom composition's `Verify` and `Review` complete through
/// the two aggregate facts, and a pass hands the re-woven tree straight back to
/// the composite gate run. A narrowed composition's line ends at its repair: the
/// cursor advances to record the tree it produced, and the member that refused
/// the original fold is the one that judges it. A failure retries the repair
/// inside its budget and wedges the composition once that is spent.
pub(super) fn reduce_composition_attempt(
    snapshot: &Snapshot,
    bloom: &BloomId,
    composition: &WorkpieceId,
    stage: StageId,
    passed: bool,
    evidence: &Evidence,
    captured: Option<CandidateRef>,
) -> Decisions {
    let composition = composition.clone();
    let Some(record) = snapshot.blooms.get(bloom).filter(|record| record.status == BloomStatus::Sealed) else {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::UnknownOrInactiveBloom));
    };
    let Some(current) = cursor(record, &composition) else {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::NotDispatched(
            composition,
        )));
    };
    if current.stage != stage {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::StageMismatch {
            expected: current.stage,
            got: stage,
        }));
    }
    if stage != StageId::Refine {
        return Decisions::rejected(Outcome::AttemptCompletedRejected(AttemptCompletedError::TerminalStage(stage)));
    }

    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];

    // A narrowed composition holds no fold of its own, so its pass re-gates
    // nothing: it advances the cursor to Verify as a record of the repair and
    // does not dispatch. No admitted fact can complete a composition at that
    // stage. The member that minted the narrowing re-verifies the repaired
    // tree — that is the only worker whose verdict a door can take. The
    // whole-bloom instance below *is* the fold, which is why its pass hands
    // the woven tree straight to the composite gates.
    if let Some(repaired) = captured.filter(|_| passed && composition.composition_parents().is_some()) {
        effects.push(Decision::AdvanceStage {
            bloom: *bloom,
            workpiece: composition.clone(),
            progress: composition_progress(StageId::Verify, 1, repaired),
        });
        // The member whose verdict minted this narrowing judged a tree it does
        // not own, and that tree has now been redone. Leaving it holding that
        // refusal strands it: it is neither resolved nor wedged and nothing is
        // in flight for it, which is the shape the liveness oracle exists to
        // catch. So it goes back to its own Verify against the repaired tree —
        // no attempt charged for the collision, because the lap it is being
        // given is the one its original verdict should have judged.
        effects.extend(reverify_after_repair(snapshot, record, bloom, &composition, repaired));
        return Decisions { outcome: Outcome::CompositionRepaired { bloom: *bloom, tree: repaired.tree }, effects };
    }

    // A repair that passed without capturing anything produced no weave, so
    // there is nothing to re-gate; it falls through to the retry path rather
    // than advancing onto the tree that was already refused.
    if let Some(woven) = captured.filter(|_| passed) {
        effects.push(Decision::RecordIntegration {
            bloom: *bloom,
            integration: Some(FoldedIntegration {
                tree: woven.tree,
                head: woven.checkout,
                // The lineage the repaired fold inherits: a repair edits the
                // composed tree, it does not re-order what went into it. A
                // landing repair holds no fold, so it starts an empty one.
                lineage: record.integration.as_ref().map_or_else(Vec::new, |held| held.lineage.clone()),
            }),
        });
        effects.push(Decision::AdvanceStage {
            bloom: *bloom,
            workpiece: composition,
            progress: composition_progress(StageId::Verify, 1, woven),
        });
        // Both gates over the repaired weave, together — the same pair the
        // completed fold dispatches, for the same reason.
        effects.extend(super::aggregate_verify::aggregate_gate_dispatches(record, *bloom, woven.tree, woven.checkout));
        return Decisions { outcome: Outcome::CompositionRepaired { bloom: *bloom, tree: woven.tree }, effects };
    }

    let weave = current.candidate.unwrap_or(CandidateRef { tree: evidence.subject, checkout: evidence.subject });
    let attempt = current.attempts + 1;
    if attempt > repair_budget(record) {
        effects.push(Decision::RecordWedge {
            bloom: *bloom,
            workpiece: composition,
            wedge: Wedge {
                stage: StageId::Refine,
                evidence: evidence.detail,
                repeated_verifiers: VerifyFailureSet::EMPTY,
            },
        });
        return Decisions {
            outcome: Outcome::CompositionWedged {
                bloom: *bloom,
                refused_at: StageId::Refine,
                question: evidence.detail,
            },
            effects,
        };
    }

    effects.extend(move_effects_with_candidate(
        *bloom,
        &composition,
        record.spec.base(),
        composition_progress(StageId::Refine, attempt, weave),
        DispatchTargets { subject: weave.tree, checkout: weave.checkout },
        Some(weave.tree),
        composition_line(record),
    ));

    Decisions { outcome: Outcome::CompositionRewoven { bloom: *bloom, refused_at: StageId::Refine, attempt }, effects }
}
