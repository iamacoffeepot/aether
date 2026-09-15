//! Arm of [`super::reduce`]'s fact dispatch (`Fact::SuppressionHold`);
//! wiring lives in `mod.rs`.
//!
//! A composed shared run settled with only the suppress gate open and the
//! lane's stated requests covering every finding (issue 6032). The member
//! parks awaiting a reviewer's sign-off: its cursor does not move, no attempt
//! and no repair roll is spent, and nothing is dispatched — the remedy is a
//! person answering through [`Fact::SuppressionDisposition`], and another lap
//! or probe would reproduce the same question verbatim.

use super::{BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress, SuppressionHoldError};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, SuppressionRequest};

/// Reduce a shared-run settlement's suppression hold against a snapshot.
///
/// The refusal ladder mirrors
/// [`reduce_surface_requested`](super::surface_request::reduce_surface_requested)'s:
/// an unknown or non-`Sealed` bloom, a workpiece that is not a member, a
/// member with no cursor, a cursor that has left `Verify`, a hold recording
/// nothing, and evidence bound to a subject other than the member's current
/// one. Empty effects beyond recording the evidence: the snapshot folds the
/// hold straight off [`Fact::SuppressionHold`](crate::Fact::SuppressionHold),
/// the way a surface request is folded from its own fact, so no new
/// [`Decision`] enters the wire-frozen decisions graph.
pub(super) fn reduce_suppression_hold(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    evidence: &Evidence,
    requests: &[SuppressionRequest],
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom).filter(|record| record.status == BloomStatus::Sealed) else {
        return Decisions::rejected(Outcome::SuppressionHoldRejected(SuppressionHoldError::UnknownOrInactiveBloom));
    };
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::SuppressionHoldRejected(SuppressionHoldError::NotAMember(
            workpiece.clone(),
        )));
    };
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::SuppressionHoldRejected(SuppressionHoldError::NotDispatched(
            workpiece.clone(),
        )));
    };
    if cursor.stage != StageId::Verify {
        return Decisions::rejected(Outcome::SuppressionHoldRejected(SuppressionHoldError::StageMismatch {
            expected: cursor.stage,
        }));
    }
    if requests.is_empty() {
        return Decisions::rejected(Outcome::SuppressionHoldRejected(SuppressionHoldError::ClosesNothing));
    }
    if !evidence.validates(&member_subject(member.scope_revision, &cursor)) {
        return Decisions::rejected(Outcome::SuppressionHoldRejected(SuppressionHoldError::EvidenceNotBound {
            expected: member_subject(member.scope_revision, &cursor),
            got: evidence.subject,
        }));
    }

    Decisions {
        outcome: Outcome::SuppressionHold {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            requests: u32::try_from(requests.len()).unwrap_or(u32::MAX),
        },
        effects: alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }],
    }
}

/// The tree the member's current lap judges: its captured candidate, or the
/// scope revision before it has produced one — the same binding a Verify
/// verdict is admitted against.
fn member_subject(scope_revision: crate::digest::Digest, cursor: &StageProgress) -> crate::digest::Digest {
    cursor.candidate.map_or(scope_revision, |candidate| candidate.tree)
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString as _;
    use alloc::vec;

    use crate::ids::{BloomId, StageId};
    use crate::reduce::{Fact, Outcome, Snapshot, SuppressionHoldError};
    use crate::testing::{compiled_resolved, digest, draft, event, membership, step, workpiece};
    use crate::values::{CandidateRef, Evidence, EvidenceKind, SuppressionRequest, VerifyFailure, VerifyFailureSet};

    fn requests() -> Vec<SuppressionRequest> {
        SuppressionRequest::normalize(vec![(
            "crates/aether-chassis-bloomery/src/bloomery/verify/batch.rs".to_string(),
            144,
            "allow(clippy::disallowed_methods)".to_string(),
            "operator tooling reading the coordinator's REST bind, not cap config".to_string(),
        )])
    }

    /// A sealed bloom whose one member has produced a candidate and is standing
    /// at `Verify` — the state a composed run settles a requesting member
    /// from.
    fn member_at_verify() -> (Snapshot, BloomId) {
        let spec = draft(1, vec![membership("alpha", 10)]).seal();
        let bloom = spec.id();
        let (snapshot, _) =
            step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "construct-alpha",
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: workpiece("alpha"),
                    stage: StageId::Construct,
                    passed: true,
                    evidence: Evidence {
                        subject: digest(10),
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(80),
                    },
                    candidate: Some(CandidateRef { tree: digest(20), checkout: digest(22) }),
                },
            ),
        );
        (snapshot, bloom)
    }

    fn hold_evidence() -> Evidence {
        Evidence { subject: digest(20), kind: EvidenceKind::VerificationResult, detail: digest(81) }
    }

    #[test]
    fn a_hold_parks_without_spending_the_members_budget() {
        // The whole point of parking rather than failing: a member whose only
        // open gate is a requested suppression has produced no defect, so
        // another attempt is not owed, a repair roll is not spent, and — the
        // issue-6032 half — no attribution probe is bought.
        let (snapshot, bloom) = member_at_verify();
        let before =
            snapshot.blooms[&bloom].progress.get(&workpiece("alpha")).copied().expect("the member is at Verify");

        let decided =
            super::reduce_suppression_hold(&snapshot, &bloom, &workpiece("alpha"), &hold_evidence(), &requests());
        assert!(matches!(decided.outcome, Outcome::SuppressionHold { requests: 1, .. }), "{:?}", decided.outcome);
        assert_eq!(decided.effects.len(), 1, "a hold records its evidence and dispatches nothing");

        let event = event(
            "hold-alpha",
            Fact::SuppressionHold {
                bloom,
                workpiece: workpiece("alpha"),
                evidence: hold_evidence(),
                requests: requests(),
            },
        );
        let after = snapshot.apply(&event, &decided, &compiled_resolved());
        let cursor = after.blooms[&bloom].progress[&workpiece("alpha")];

        assert_eq!(cursor.attempts, before.attempts, "a park spends no attempt");
        assert_eq!(cursor.repair_rolls, before.repair_rolls, "a park spends no repair roll");
        assert_eq!(cursor.stage, before.stage, "a park does not move the cursor");
        assert_eq!(cursor.candidate, before.candidate, "a park keeps the candidate the grant resumes from");
        let held = after.awaiting_suppression(&bloom, &workpiece("alpha")).expect("the hold is recorded");
        assert_eq!(held.requests.len(), 1, "the reviewer sees what the candidate asked for");
        assert_eq!(held.evidence, digest(81));
    }

    #[test]
    fn a_hold_recording_nothing_is_refused() {
        // An answer that closes nothing is refused at the disposition door, so
        // a hold that records nothing is refused here too — the journal never
        // carries a park with nothing for a reviewer to answer.
        let (snapshot, bloom) = member_at_verify();

        let decided = super::reduce_suppression_hold(&snapshot, &bloom, &workpiece("alpha"), &hold_evidence(), &[]);

        assert!(
            matches!(decided.outcome, Outcome::SuppressionHoldRejected(SuppressionHoldError::ClosesNothing)),
            "{:?}",
            decided.outcome
        );
        assert!(decided.effects.is_empty(), "a refused hold records nothing");
    }

    #[test]
    fn a_hold_over_a_stale_subject_is_refused() {
        // The evidence must bind the member's current candidate: a step over a
        // superseded tree cannot park the lap that replaced it.
        let (snapshot, bloom) = member_at_verify();
        let stale = Evidence { subject: digest(19), kind: EvidenceKind::VerificationResult, detail: digest(81) };

        let decided = super::reduce_suppression_hold(&snapshot, &bloom, &workpiece("alpha"), &stale, &requests());

        assert!(
            matches!(decided.outcome, Outcome::SuppressionHoldRejected(SuppressionHoldError::EvidenceNotBound { .. })),
            "{:?}",
            decided.outcome
        );
    }

    #[test]
    fn a_hold_for_a_member_still_constructing_is_refused() {
        // A hold names a Verify-step settlement. A member still at Construct
        // has no verify behind it, so the report is for a step that never ran.
        let spec = draft(1, vec![membership("alpha", 10)]).seal();
        let bloom = spec.id();
        let (snapshot, _) =
            step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
        assert_eq!(
            snapshot.blooms[&bloom].progress.get(&workpiece("alpha")).map(|cursor| cursor.stage),
            Some(StageId::Construct),
            "sealing dispatches the entry stage"
        );

        let decided =
            super::reduce_suppression_hold(&snapshot, &bloom, &workpiece("alpha"), &hold_evidence(), &requests());

        assert!(
            matches!(decided.outcome, Outcome::SuppressionHoldRejected(SuppressionHoldError::StageMismatch { .. })),
            "{:?}",
            decided.outcome
        );
    }

    #[test]
    fn a_hold_racing_a_red_verdict_that_ejected_the_member_is_refused() {
        // The composed step settled while the member's own red verdict was
        // already withdrawing it. There is no cursor left to park and no
        // candidate carrying the requests — but the bloom walks on with its
        // sibling, so the hold is refused for the member, not the bloom.
        let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
        let bloom = spec.id();
        let (snapshot, _) =
            step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "construct-alpha",
                Fact::AttemptCompleted {
                    bloom,
                    workpiece: workpiece("alpha"),
                    stage: StageId::Construct,
                    passed: true,
                    evidence: Evidence {
                        subject: digest(10),
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(80),
                    },
                    candidate: Some(CandidateRef { tree: digest(20), checkout: digest(22) }),
                },
            ),
        );
        let (snapshot, _) = step(
            &snapshot,
            &event(
                "verify-alpha",
                Fact::VerifyFailed {
                    bloom,
                    workpiece: workpiece("alpha"),
                    evidence: hold_evidence(),
                    failed_verifiers: VerifyFailureSet::one(VerifyFailure::Clippy),
                    findings: "clippy is red".to_string(),
                },
            ),
        );
        assert!(snapshot.blooms.get(&bloom).is_some(), "the bloom walks on with its sibling");
        assert!(
            snapshot.blooms[&bloom].progress.get(&workpiece("alpha")).is_none(),
            "the sealed eject-on-red disposition withdrew the member"
        );

        let decided =
            super::reduce_suppression_hold(&snapshot, &bloom, &workpiece("alpha"), &hold_evidence(), &requests());

        assert!(
            matches!(decided.outcome, Outcome::SuppressionHoldRejected(SuppressionHoldError::NotDispatched(_))),
            "{:?}",
            decided.outcome
        );
    }
}
