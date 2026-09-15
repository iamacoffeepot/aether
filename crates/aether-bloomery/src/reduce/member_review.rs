//! Arm of [`super::reduce`]'s fact dispatch (`Fact::ReviewFailed`); wiring
//! lives in `mod.rs`.
//!
//! The member line's terminal judge finding against one candidate (ADR-0221).
//!
//! Deliberately the same shape as [`super::verify`]'s red arm, because it is
//! the same question one gate later: a member at the end of its line did not
//! pass, and the bloom's sealed
//! [`RedVerify`](crate::values::RedVerify) disposition already says what a
//! member that did not pass is worth — leave, or one directed repair lap. What
//! differs is only what the member is told: a verify hands back typed verifier
//! identities, a review hands back prose.

use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects};
use super::eject::{eject, ejection_reason};
use super::{BloomRecord, BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress, VerifyFailedError};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, Membership, RedVerify, VerifyFailureSet, Wedge};

/// Reduce one admitted failing member-`Review` verdict (ADR-0221).
///
/// Three admissions first — the bloom is sealed and holds this member, the
/// member's cursor stands at `Review`, and the evidence binds the candidate the
/// judge was shown — then the sealed disposition decides. `Eject` withdraws the
/// member carrying the judge's findings; `Refine` spends one repair roll and
/// re-enters the repair lane, which returns through `Verify` (the lap changed
/// the code, so the compiler answers again) and on to `Review` from there.
///
/// Reuses [`VerifyFailedError`] for its refusals rather than minting a parallel
/// set: the three questions are the same three, and a second enum would make an
/// operator learn two spellings of one refusal.
pub(super) fn reduce_review_failed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    evidence: &Evidence,
    findings: &str,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom).filter(|record| record.status == BloomStatus::Sealed) else {
        return Decisions::rejected(Outcome::ReviewFailedRejected(VerifyFailedError::UnknownOrInactiveBloom));
    };
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::ReviewFailedRejected(VerifyFailedError::NotAMember(workpiece.clone())));
    };
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::ReviewFailedRejected(VerifyFailedError::NotDispatched(workpiece.clone())));
    };
    if cursor.stage != StageId::Review {
        return Decisions::rejected(Outcome::ReviewFailedRejected(VerifyFailedError::StageMismatch {
            expected: cursor.stage,
        }));
    }

    let (subject, checkout) = cursor.candidate.map_or_else(
        || (member.scope_revision, super::splice::member_construct_base(record, workpiece)),
        |candidate| (candidate.tree, candidate.checkout),
    );
    if !evidence.validates(&subject) {
        return Decisions::rejected(Outcome::ReviewFailedRejected(VerifyFailedError::EvidenceNotBound {
            expected: subject,
            got: evidence.subject,
        }));
    }

    let effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    if record.red_verify == RedVerify::Eject {
        return eject(
            snapshot,
            record,
            bloom,
            workpiece,
            &ejection_reason("its review found against the candidate", VerifyFailureSet::EMPTY, evidence, findings),
            effects,
        );
    }

    repair_lap(record, *bloom, member, &cursor, evidence, DispatchTargets { subject, checkout }, effects)
}

/// Re-enter `Refine` on the judge's findings, or wedge once the member has spent
/// the sealed `Review` budget on them.
///
/// The roll ledger is the ADR-0178 one, read at `Review` rather than at
/// `Verify`: a review verdict names no verifier identity, so there is no seen
/// set to intersect and nothing finer than "this member was sent back again" to
/// count. The ceiling is the sealed catalog's `Review` retry budget, and it has
/// to be a ceiling rather than an open loop — a repair lap that hands back a
/// tree the judge already found against would otherwise re-enter forever, and
/// under `Eject` the question never arises because the first red verdict is the
/// last one.
fn repair_lap(
    record: &BloomRecord,
    bloom: BloomId,
    member: &Membership,
    cursor: &StageProgress,
    evidence: &Evidence,
    targets: DispatchTargets,
    mut effects: Vec<Decision>,
) -> Decisions {
    let workpiece = &member.workpiece;
    let rolls = cursor.repair_rolls + 1;

    if rolls >= record.stage_catalog.retry_budget_of(StageId::Review).unwrap_or(1) {
        let progress = StageProgress { repair_rolls: rolls, ..*cursor };
        // The cursor write is deliberately unpaired with a dispatch, and the
        // wedge that follows restores the terminal marker `AdvanceStage`
        // clears — the same ordering the terminal Verify verdict relies on.
        effects.push(Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress });
        effects.push(Decision::RecordWedge {
            bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge {
                stage: StageId::Review,
                evidence: evidence.detail,
                repeated_verifiers: VerifyFailureSet::EMPTY,
            },
        });
        return Decisions {
            outcome: Outcome::AttemptWedged {
                bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Review,
                repeated_verifiers: VerifyFailureSet::EMPTY,
            },
            effects,
        };
    }

    let progress = StageProgress {
        stage: StageId::Refine,
        attempts: 1,
        repair_rolls: rolls,
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
        ..*cursor
    };
    effects.extend(move_effects(
        bloom,
        workpiece,
        member.scope_revision,
        &progress,
        targets,
        SealedLine::of(record, member),
    ));

    Decisions { outcome: Outcome::RefineReentered { bloom, workpiece: workpiece.clone(), rolls }, effects }
}
