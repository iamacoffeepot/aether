//! The evidence log and the pending-decision hold: admitting non-integrating
//! evidence, and the adopting answer that releases a parked question
//! (ADR-0151).

use super::{AdmitEvidenceError, AdoptAnswerError, BloomStatus, Decision, Decisions, Outcome, Snapshot};
use crate::digest::digest_of;
use crate::ids::BloomId;
use crate::ids::StageId;
use crate::values::{Evidence, EvidenceKind, Statement};

/// Admit non-integrating evidence into a bloom's evidence log (ADR-0151). Runs
/// the same active-bloom guard `reduce_integrate` does — evidence records only
/// against a `Sealed` bloom, before it resolves — and the same
/// bind-to-its-own-class refusal: a resolving
/// [`ResolutionClaim`](crate::ResolutionClaim) enters through
/// [`Fact::Integrate`](crate::Fact::Integrate) and an [`EvidenceKind::Approval`]
/// seals a member, so
/// neither is bound to the free evidence-log door. The five non-integrating
/// classes (`VerificationResult`, `ReviewFinding`, `StudyRecord`, `Question`,
/// `SuppressionRequest`)
/// are what this log records; a mis-routed integrating/approval class is
/// [`AdmitEvidenceError::EvidenceNotBound`]. A `Question` entry additionally
/// derives a pending-decision hold in the fold (see
/// [`BloomRecord::holds`](crate::BloomRecord::holds)).
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
            // A candidate's stated case for its suppressions (ADR-0193). It
            // raises no hold and advances nothing: the lane already passed, and
            // the request is a question for a reviewer who does not exist yet.
            | EvidenceKind::SuppressionRequest
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

/// Adopt an answer to a parked question (ADR-0151,
/// [`Fact::AdoptAnswer`](crate::Fact::AdoptAnswer)).
///
/// The reducer's structural gate: the bloom is active, the answer is
/// instruction-capable (an author signature — only that provenance becomes
/// intent), and its `parents` name a digest that is an open hold on the bloom.
/// On admit it releases that hold and re-dispatches the held stage with the
/// answer digest in the attempt's input closure.
///
/// The cryptographic `verify_authority` check is the host answer route's,
/// upstream of admission (the reducer holds no key material) — the same trust
/// split the intake broker uses for evidence. That route verifies the signature
/// *bound to the question digest the request named*
/// ([`AuthorityDoor::Answer`](crate::AuthorityDoor), ADR-0182), and — because
/// [`Fact::AdoptAnswer`](crate::Fact::AdoptAnswer) carries no question field of
/// its own — it additionally refuses any answer whose `parents` is not exactly
/// that one question. Those two checks together are what make the scan below a
/// re-check: the route has already proved that the only digest this scan can
/// select is the digest the signature was bound to.
///
/// The route-side `parents` refusal is load-bearing, not belt-and-braces. A
/// signature bound to the path question proves the *envelope* is genuine for
/// that question; it says nothing about `parents`, which sits outside the
/// signature. Verifying alone would leave the submitter supplying both halves of
/// the equality — a genuine envelope signed at `(Answer, Q1)` posted to
/// `.../answer/{Q1}` verifies, and its rewritten `parents` is what this scan
/// would then act on, releasing a hold nobody signed for.
///
/// The scan itself stays because the reducer is key-free: on replay it is the
/// only binding evaluable here, and dropping it would trade a structural check
/// for nothing. It selects the first parent that is an open hold in the
/// submitter's `parents` order — *not* in digest order; `holds` is a
/// [`BTreeSet`](alloc::collections::BTreeSet), so the membership test is
/// deterministic, but the iteration order that picks the winner is the
/// statement's. That is exactly why the route requires equality with a
/// single-element list rather than membership: `[Q2, Q1]` contains the path
/// question and would still release `Q2`. With the route in front, a multi-hold
/// bloom is unambiguous — a parked question raises one hold per member, so
/// multi-hold is the normal case, and every admitted answer names exactly one of
/// them.
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
    // the first parent that is an open hold is the released question, in the
    // submitter's parents order (`holds` is a BTreeSet, so the membership test
    // is deterministic; the order that picks the winner is the statement's).
    // The host route admits only a single-element `parents` equal to the
    // question its signature is bound to, so there is exactly one candidate
    // here — see the doc comment above for why membership would not be enough.
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
            Decision::RecordAggregateVerifyRoll { bloom: *bloom, rolls: 0 },
        ];
        // The park keeps the fold held, so the re-armed gates dispatch from it
        // directly; a missing fold (unreachable through the park path) just
        // resets the cycle and leaves the re-fold to dispatch them.
        if let Some(integration) = &record.integration {
            // Only the gates that have not passed on the held fold. The two run
            // concurrently, so the bloom can be parked at either ceiling with
            // its sibling's pass already recorded, and re-dispatching a gate
            // that passed would spend a lane to re-learn a verdict the record
            // holds. Both roll cursors reset above, so a bloom parked at the
            // mechanical ceiling gets a real cycle rather than a dispatch that
            // parks again on arrival.
            //
            // Roll 1 in each order, not the record's stored counts: the same
            // decision set zeroes both cursors and the snapshot has not folded
            // that yet. Each order goes through the same gate helper the
            // ordinary dispatches use, so a hold taken before the owner re-arms
            // still withholds the paid lane (#5100).
            if !record.aggregate_passed.contains(&StageId::AggregateVerify) {
                effects.extend(super::aggregate_verify::gate_aggregate(
                    record,
                    *bloom,
                    crate::AGGREGATE_VERIFY_GATE,
                    StageId::AggregateVerify,
                    super::aggregate_verify::owed_aggregate_verify(
                        record,
                        *bloom,
                        integration.tree,
                        integration.head,
                        1,
                    ),
                ));
            }
            if !record.aggregate_passed.contains(&StageId::AggregateReview) {
                effects.extend(super::aggregate_verify::gate_aggregate(
                    record,
                    *bloom,
                    crate::AGGREGATE_REVIEW_GATE,
                    StageId::AggregateReview,
                    super::aggregate_verify::owed_aggregate_review(
                        record,
                        *bloom,
                        integration.tree,
                        integration.head,
                        1,
                    ),
                ));
            }
        }
        return Decisions { outcome: Outcome::AnswerAdopted { bloom: *bloom, question }, effects };
    }
    Decisions {
        outcome: Outcome::AnswerAdopted { bloom: *bloom, question },
        effects: alloc::vec![
            Decision::ReleaseHold { bloom: *bloom, question },
            Decision::RedispatchStage { bloom: *bloom, question, answer: answer_digest, words: answer.words.clone() },
        ],
    }
}
