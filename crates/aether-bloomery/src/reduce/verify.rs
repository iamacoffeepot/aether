//! Typed terminal-Verify failure accounting (ADR-0178).

use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects, wedged};
use super::{BloomRecord, BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress, VerifyFailedError};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, Membership, VerifyFailureSet, Wedge};

/// Reduce one admitted failing member-Verify verdict.
///
/// For current failures `F` and the member's seen set `S`, `R = F ∩ S`
/// decides whether this verdict spends one repair roll, while `S ∪ F` becomes
/// the durable cursor history. A verdict spends at most one roll however many
/// repeated identities it contains.
///
/// `F` empty is not that shape at all — it is the *absence* of a verdict, and
/// routes to [`unjudged_verify`] instead of into the repair accounting above.
pub(super) fn reduce_verify_failed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    evidence: &Evidence,
    failed_verifiers: VerifyFailureSet,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::UnknownOrInactiveBloom));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::NotAMember(workpiece.clone())));
    };
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::NotDispatched(workpiece.clone())));
    };
    if cursor.stage != StageId::Verify {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::StageMismatch {
            expected: cursor.stage,
        }));
    }

    let (subject, checkout) = cursor
        .candidate
        .map_or_else(|| (member.scope_revision, record.spec.base()), |candidate| (candidate.tree, candidate.checkout));
    if !evidence.validates(&subject) {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::EvidenceNotBound {
            expected: subject,
            got: evidence.subject,
        }));
    }
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];

    // Binding first, emptiness second: an unjudged verdict still re-dispatches
    // work, so it has to name the subject it was dispatched against before it is
    // allowed to move the member at all.
    if failed_verifiers.is_empty() {
        return unjudged_verify(
            record,
            *bloom,
            member,
            &cursor,
            evidence,
            DispatchTargets { subject, checkout },
            effects,
        );
    }

    let repeated_verifiers = failed_verifiers.intersection(cursor.seen_verify_failures);
    let seen_verify_failures = cursor.seen_verify_failures.union(failed_verifiers);
    let rolls = cursor.repair_rolls + u32::from(!repeated_verifiers.is_empty());

    // The loop is bounded by N + B: V1 has N = 8 identities, so at most eight
    // failed verdicts can add a new identity without spending a roll; at most B
    // later verdicts can spend the sealed Verify budget before this member wedges.
    if !repeated_verifiers.is_empty() && rolls >= record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1) {
        let progress = StageProgress {
            stage: StageId::Verify,
            attempts: cursor.attempts,
            candidate: cursor.candidate,
            repair_rolls: rolls,
            seen_verify_failures,
            fold_checkpoint: cursor.fold_checkpoint,
            fold_conflict_evidence: None,
        };
        // Persist the union even on the terminal verdict. This cursor write is
        // intentionally not paired with a dispatch; the following RecordWedge
        // restores the terminal marker after AdvanceStage clears any stale one.
        effects.push(Decision::AdvanceStage { bloom: *bloom, workpiece: workpiece.clone(), progress });
        effects.push(Decision::RecordWedge {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge { stage: StageId::Verify, evidence: evidence.detail, repeated_verifiers },
        });
        return Decisions {
            outcome: Outcome::AttemptWedged {
                bloom: *bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Verify,
                repeated_verifiers,
            },
            effects,
        };
    }

    // The member is still in whatever fold round it was in (#4952): a repair lap
    // changes the candidate, not which folded tree the candidate has to land on.
    let progress = StageProgress {
        stage: StageId::Refine,
        attempts: 1,
        candidate: cursor.candidate,
        repair_rolls: rolls,
        seen_verify_failures,
        fold_checkpoint: cursor.fold_checkpoint,
        fold_conflict_evidence: None,
    };
    effects.extend(move_effects(
        *bloom,
        workpiece,
        member.scope_revision,
        progress,
        DispatchTargets { subject, checkout },
        SealedLine::of(record, member),
    ));
    Decisions { outcome: Outcome::RefineReentered { bloom: *bloom, workpiece: workpiece.clone(), rolls }, effects }
}

/// Re-run the gate over a candidate no gate ever judged, or wedge once the
/// member's `Verify` budget is spent.
///
/// A failing member `Verify` that names no verifier is not a failure — ADR-0178
/// gives every real one at least one identity, so an empty set is a lane that
/// died before the umbrella rendered a verdict: killed mid-build, cancelled on
/// its execution limit, gone without leaving readable evidence. The candidate it
/// was dispatched against is untouched.
///
/// That is why this is not a repair. `Refine` hands a model a worktree checked
/// out *at* the candidate and asks it to fix the failures the gate named; with
/// no failures to name, a lane that correctly answers "this work order is
/// already satisfied" changes nothing, captures no diff, and is recorded as one
/// more failed attempt — three laps of that spends a member's whole repair
/// budget on a gate that never ran. What has to happen again is the
/// *verification*, so the retry is `Verify`'s own: its budget, its `attempts`
/// counter, the same targets the failed dispatch aimed at.
///
/// Nothing is charged to the ADR-0178 accounting either. `repair_rolls` and
/// `seen_verify_failures` both pass through unchanged — an attempt that rendered
/// no verdict saw no verifier fail, and folding it into the seen set would let
/// the *next* honest failure read as a repeat and wedge the member for a defect
/// it never had.
///
/// Exhaustion wedges at `Verify` carrying the unjudged attempt's own evidence
/// (the timeout record, the synthesised missing-evidence artifact) and an empty
/// `repeated_verifiers` — which is itself the operator's discriminator, since
/// the repeat rule above always wedges naming the identities that repeated. An
/// empty set on a `Verify` wedge reads as "the gate never answered".
fn unjudged_verify(
    record: &BloomRecord,
    bloom: BloomId,
    member: &Membership,
    cursor: &StageProgress,
    evidence: &Evidence,
    targets: DispatchTargets,
    mut effects: Vec<Decision>,
) -> Decisions {
    let workpiece = &member.workpiece;
    let budget = record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1);
    if cursor.attempts >= budget {
        return wedged(bloom, workpiece, StageId::Verify, evidence, effects);
    }

    let attempt = cursor.attempts + 1;
    let progress = StageProgress {
        stage: StageId::Verify,
        attempts: attempt,
        candidate: cursor.candidate,
        repair_rolls: cursor.repair_rolls,
        seen_verify_failures: cursor.seen_verify_failures,
        fold_checkpoint: cursor.fold_checkpoint,
        fold_conflict_evidence: None,
    };
    effects.extend(move_effects(
        bloom,
        workpiece,
        member.scope_revision,
        progress,
        targets,
        SealedLine::of(record, member),
    ));

    Decisions {
        outcome: Outcome::AttemptRetried { bloom, workpiece: workpiece.clone(), stage: StageId::Verify, attempt },
        effects,
    }
}
