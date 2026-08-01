//! The evidence log and the pending-decision hold: admitting non-integrating
//! evidence, and the adopting answer that releases a parked question
//! (ADR-0151).

use super::{AdmitEvidenceError, AdoptAnswerError, BloomStatus, Decision, Decisions, Outcome, Snapshot};
use crate::digest::digest_of;
use crate::ids::BloomId;
use crate::values::{Evidence, EvidenceKind, Statement, Transformation};

/// Admit non-integrating evidence into a bloom's evidence log (ADR-0151). Runs
/// the same active-bloom guard `reduce_integrate` does — evidence records only
/// against a `Sealed` bloom, before it resolves — and the same
/// bind-to-its-own-class refusal: a resolving [`ResolutionClaim`] enters through
/// [`Fact::Integrate`] and an [`EvidenceKind::Approval`] seals a member, so
/// neither is bound to the free evidence-log door. The four non-integrating
/// classes (`VerificationResult`, `ReviewFinding`, `StudyRecord`, `Question`)
/// are what this log records; a mis-routed integrating/approval class is
/// [`AdmitEvidenceError::EvidenceNotBound`]. A `Question` entry additionally
/// derives a pending-decision hold in the fold (see [`BloomRecord::holds`]).
/// The evidence carries its own
/// subject digest, so no separate candidate binding is threaded through the
/// fact (unlike integrate, which binds a claim's evidence to its candidate).
pub(super) fn reduce_admit_evidence(snapshot: &Snapshot, bloom: &BloomId, evidence: &Evidence) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::AdmitEvidenceRejected(AdmitEvidenceError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::AdmitEvidenceRejected(AdmitEvidenceError::UnknownOrInactiveBloom));
    }
    // Only the non-integrating classes bind to the evidence log — a resolution
    // claim integrates and an approval seals, each through its own door
    // (ADR-0151: a `ResolutionClaim` never enters through `AdmitEvidence`).
    if !matches!(
        evidence.kind,
        EvidenceKind::VerificationResult
            | EvidenceKind::ReviewFinding
            | EvidenceKind::StudyRecord
            | EvidenceKind::Question
    ) {
        return Decisions::rejected(Outcome::AdmitEvidenceRejected(AdmitEvidenceError::EvidenceNotBound));
    }
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    // A question bound to the held fold's tree is the aggregate review itself
    // parking — the ADR-0153 "findings are contested" branch. Mark it as the
    // bloom-scope park so the adopting answer re-arms the review cycle rather
    // than routing down the member-stage redispatch path.
    if evidence.kind == EvidenceKind::Question
        && let Some(integration) = &record.integration
        && evidence.subject == integration.tree
    {
        effects.push(Decision::RecordReviewPark { bloom: *bloom, question: Some(evidence.detail) });
    }
    Decisions { outcome: Outcome::EvidenceAdmitted { bloom: *bloom, subject: evidence.subject }, effects }
}

/// Adopt an answer to a parked question (ADR-0151, [`Fact::AdoptAnswer`]).
///
/// The reducer's structural gate: the bloom is active, the answer is
/// instruction-capable (an author signature — only that provenance becomes
/// intent), and its `parents` name a digest that is an open hold on the bloom.
/// On admit it releases that hold and re-dispatches the held stage with the
/// answer digest in the attempt's input closure. The cryptographic
/// `verify_authority` check is the host answer route's, upstream of admission
/// (the reducer holds no key material) — the same trust split the intake broker
/// uses for evidence, where the reducer re-checks binding but not the signature.
///
/// An answer whose parents name several open holds releases the first one in
/// digest order; a parked question raises one hold per member, so the common
/// case names exactly one.
pub(super) fn reduce_adopt_answer(snapshot: &Snapshot, bloom: &BloomId, answer: &Statement) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::AdoptAnswerRejected(AdoptAnswerError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::AdoptAnswerRejected(AdoptAnswerError::UnknownOrInactiveBloom));
    }
    // Only an author signature can become intent — a non-author statement can
    // never adopt a question (ADR-0149 §The value vocabulary).
    if !answer.is_instruction_capable() {
        return Decisions::rejected(Outcome::AdoptAnswerRejected(AdoptAnswerError::NotInstructionCapable));
    }
    // The answer adopts a question by naming its exact digest in its parents;
    // the first parent that is an open hold is the released question (holds is a
    // BTreeSet, so the scan is deterministic).
    let Some(question) = answer.parents.iter().find(|parent| record.holds.contains(parent)).copied() else {
        return Decisions::rejected(Outcome::AdoptAnswerRejected(AdoptAnswerError::NoMatchingHold));
    };
    let answer_digest = digest_of(answer);
    // The bloom-scope park adopts differently (ADR-0153): the owner's answer
    // re-arms the review cycle — the roll cursor resets and, with the fold
    // still held from the park, a fresh full review dispatches. The owner
    // bought the new cycle explicitly; the machine still never buys its own
    // third roll. A member-scope question re-dispatches the held stage as
    // before.
    if record.review_park == Some(question) {
        let mut effects = alloc::vec![
            Decision::ReleaseHold { bloom: *bloom, question },
            Decision::RecordReviewPark { bloom: *bloom, question: None },
            Decision::RecordAggregateRoll { bloom: *bloom, rolls: 0 },
        ];
        // The park keeps the fold held, so the re-armed review dispatches from
        // it directly; a missing fold (unreachable through the park path) just
        // resets the cycle and leaves the re-fold to dispatch the review.
        if let Some(integration) = &record.integration {
            effects.push(Decision::DispatchAggregateReview {
                bloom: *bloom,
                transformation: Transformation::for_aggregate_review(integration.tree, integration.head),
                roll: 1,
            });
        }
        return Decisions { outcome: Outcome::AnswerAdopted { bloom: *bloom, question }, effects };
    }
    Decisions {
        outcome: Outcome::AnswerAdopted { bloom: *bloom, question },
        effects: alloc::vec![
            Decision::ReleaseHold { bloom: *bloom, question },
            Decision::RedispatchStage { bloom: *bloom, question, answer: answer_digest },
        ],
    }
}
