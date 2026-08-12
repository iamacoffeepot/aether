//! Typed terminal-Verify failure accounting (ADR-0178).

use super::attempt::{DispatchTargets, SealedLine, move_effects};
use super::{BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress, VerifyFailedError};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, VerifyFailureSet, Wedge};

/// Reduce one admitted failing member-Verify verdict.
///
/// For current failures `F` and the member's seen set `S`, `R = F ∩ S`
/// decides whether this verdict spends one repair roll, while `S ∪ F` becomes
/// the durable cursor history. A verdict spends at most one roll however many
/// repeated identities it contains.
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
    if failed_verifiers.is_empty() {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::EmptyFailures));
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

    let repeated_verifiers = failed_verifiers.intersection(cursor.seen_verify_failures);
    let seen_verify_failures = cursor.seen_verify_failures.union(failed_verifiers);
    let rolls = cursor.repair_rolls + u32::from(!repeated_verifiers.is_empty());
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];

    // The loop is bounded by N + B: V1 has N = 7 identities, so at most seven
    // failed verdicts can add a new identity without spending a roll; at most B
    // later verdicts can spend the sealed Verify budget before this member wedges.
    if !repeated_verifiers.is_empty() && rolls >= record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1) {
        let progress = StageProgress {
            stage: StageId::Verify,
            attempts: cursor.attempts,
            candidate: cursor.candidate,
            repair_rolls: rolls,
            seen_verify_failures,
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

    let progress = StageProgress {
        stage: StageId::Refine,
        attempts: 1,
        candidate: cursor.candidate,
        repair_rolls: rolls,
        seen_verify_failures,
    };
    effects.extend(move_effects(
        *bloom,
        workpiece,
        member.scope_revision,
        progress,
        DispatchTargets { subject, checkout },
        SealedLine { configs: member.configs.layered_over(record.spec.configs()), catalog: &record.stage_catalog },
    ));
    Decisions { outcome: Outcome::RefineReentered { bloom: *bloom, workpiece: workpiece.clone(), rolls }, effects }
}
