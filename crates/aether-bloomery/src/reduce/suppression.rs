//! Arm of [`super::reduce`]'s fact dispatch (`Fact::SuppressionDisposition`);
//! wiring lives in `mod.rs`.
//!
//! The reviewer's half of ADR-0193. The lane states its case and continues; the
//! answer arrives later, from a person, and this is what it does to the member.

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate};
use super::{BloomStatus, Decision, Decisions, Outcome, Snapshot, StageProgress, SuppressionDispositionError};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{CandidateRef, SuppressionDisposition, VerifyFailure, VerifyFailureSet, Wedge};

/// Reduce one reviewer answer to a member's standing suppression requests.
///
/// A **grant** decides nothing about the line. The candidate already passed its
/// gates — `verify.suppress` answered `pass` the moment the requests were
/// stated — so the member is exactly where the grant found it and there is
/// nothing to dispatch. What the grant produces is the record of who granted
/// it, which rides the admitted fact into the snapshot rather than through an
/// effect: no [`Decision`] variant is added, so the wire-frozen decision graph
/// and its pinned fixture are untouched. That is the shape ADR-0204's lease
/// table established.
///
/// A **denial** re-opens the member at `Refine` and spends a repair roll,
/// because a denied request is a candidate carrying a suppression it may not
/// keep — the same standing a failing `verify.suppress` verdict would have
/// given it, arriving late. The roll is charged for the same reason it is
/// charged on a bounced lap: the work has to be done again, and a member that
/// cannot get past the gate inside its budget wedges rather than looping.
///
/// A member that has already integrated is re-opened too, its claim revoked
/// first. ADR-0191's rule that a reviewed member is immutable governs what the
/// *aggregate review* may re-route; it does not govern a person's answer to a
/// question that member asked, and the candidate under the claim is the one
/// carrying the refused suppression. ADR-0193 states the cost plainly: a
/// denial arriving after integration spends budget the bloom had banked.
pub(super) fn reduce_suppression_disposition(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    disposition: &SuppressionDisposition,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom).filter(|record| record.status == BloomStatus::Sealed) else {
        return Decisions::rejected(Outcome::SuppressionRejected(SuppressionDispositionError::UnknownOrInactiveBloom));
    };
    if !disposition.is_well_formed() {
        return Decisions::rejected(Outcome::SuppressionRejected(malformed(disposition)));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::SuppressionRejected(SuppressionDispositionError::NotAMember(
            workpiece.clone(),
        )));
    };
    if record.withdrawn.contains_key(workpiece) {
        return Decisions::rejected(Outcome::SuppressionRejected(SuppressionDispositionError::AlreadyWithdrawn(
            workpiece.clone(),
        )));
    }

    if !disposition.reopens() {
        return Decisions {
            outcome: Outcome::SuppressionAnswered { bloom: *bloom, workpiece: workpiece.clone(), reopened: false },
            effects: alloc::vec![],
        };
    }

    // Read before the revoke effect applies — the reducer is pure over
    // `snapshot`, so an integrated member's claim is still the only record of
    // the tree it resolved on. A member with a cursor keeps that candidate.
    let cursor = record.progress.get(workpiece).copied();
    let claimed = record.claims.get(workpiece);
    let candidate = cursor
        .and_then(|progress| progress.candidate)
        .or_else(|| claimed.map(|claim| CandidateRef { tree: claim.candidate, checkout: record.spec.base() }));
    let Some(candidate) = candidate else {
        return Decisions::rejected(Outcome::SuppressionRejected(SuppressionDispositionError::NoCandidate(
            workpiece.clone(),
        )));
    };

    let mut effects = alloc::vec![];
    if claimed.is_some() {
        effects.push(Decision::RevokeResolution { bloom: *bloom, workpiece: workpiece.clone() });
    }

    // Charged against the ADR-0178 repair accounting under `verify.suppress`'s
    // own identity, which is the gate the candidate is being sent back to
    // clear. Folding it into the seen set is what makes a second denial of the
    // same member read as a repeat rather than as a fresh identity.
    let rolls = cursor.map_or(0, |progress| progress.repair_rolls) + 1;
    let seen_verify_failures = cursor
        .map_or(VerifyFailureSet::EMPTY, |progress| progress.seen_verify_failures)
        .union(VerifyFailureSet::one(VerifyFailure::Suppress));
    if rolls >= record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1) {
        effects.push(Decision::RecordWedge {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge {
                stage: StageId::Verify,
                evidence: candidate.tree,
                repeated_verifiers: VerifyFailureSet::one(VerifyFailure::Suppress),
            },
        });
        return Decisions {
            outcome: Outcome::AttemptWedged {
                bloom: *bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Verify,
                repeated_verifiers: VerifyFailureSet::one(VerifyFailure::Suppress),
            },
            effects,
        };
    }

    let progress = StageProgress {
        stage: StageId::Refine,
        attempts: 1,
        candidate: Some(candidate),
        repair_rolls: rolls,
        seen_verify_failures,
        fold_checkpoint: cursor.and_then(|progress| progress.fold_checkpoint),
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
    };
    effects.extend(move_effects_with_candidate(
        *bloom,
        workpiece,
        member.scope_revision,
        progress,
        DispatchTargets { subject: candidate.tree, checkout: candidate.checkout },
        Some(candidate.tree),
        SealedLine::of(record, member),
    ));

    Decisions {
        outcome: Outcome::SuppressionAnswered { bloom: *bloom, workpiece: workpiece.clone(), reopened: true },
        effects,
    }
}

/// Which of the three well-formedness rules a disposition broke, in declaration
/// order, so the door tells the caller the first thing wrong with it.
fn malformed(disposition: &SuppressionDisposition) -> SuppressionDispositionError {
    if disposition.requests.is_empty() {
        SuppressionDispositionError::ClosesNothing
    } else if disposition.reason.trim().is_empty() {
        SuppressionDispositionError::BlankReason
    } else {
        SuppressionDispositionError::BlankOperator
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString as _;
    use alloc::vec;

    use crate::ids::{BloomId, StageId};
    use crate::reduce::{Decision, Fact, Outcome, Snapshot, SuppressionDispositionError};
    use crate::testing::{digest, draft, event, membership, step, workpiece};
    use crate::values::{
        CandidateRef, Evidence, EvidenceKind, SuppressionDisposition, SuppressionVerdict, VerifyFailure,
    };

    fn answer(verdict: SuppressionVerdict) -> SuppressionDisposition {
        SuppressionDisposition {
            requests: vec![digest(41)],
            verdict,
            reason: "the policy blesses this read".to_string(),
            operator: "owner".to_string(),
        }
    }

    /// A sealed bloom whose one member has produced a candidate and is standing
    /// at `Verify` — the state a candidate carrying a requested suppression is
    /// in while it waits for an answer.
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

    fn answered(verdict: SuppressionVerdict) -> (Snapshot, crate::Decisions) {
        let (snapshot, bloom) = member_at_verify();
        step(
            &snapshot,
            &event(
                "answer-alpha",
                Fact::SuppressionDisposition { bloom, workpiece: workpiece("alpha"), disposition: answer(verdict) },
            ),
        )
    }

    #[test]
    fn a_grant_decides_nothing_about_the_line() {
        // The candidate already passed: `verify.suppress` answered `pass` when
        // the request was stated. A grant that moved a cursor would re-run a
        // stage nobody failed, and re-dispatching a passing member is how a
        // cheap approval becomes an expensive one.
        let (_, decided) = answered(SuppressionVerdict::Granted);

        assert!(
            matches!(decided.outcome, Outcome::SuppressionAnswered { reopened: false, .. }),
            "{:?}",
            decided.outcome
        );
        assert!(decided.effects.is_empty(), "a grant dispatches nothing: {:?}", decided.effects);
    }

    #[test]
    fn a_denial_re_opens_the_member_at_refine_on_its_own_candidate() {
        // The lap has to check out the tree carrying the refused suppression.
        // Dispatching against the scope revision instead would bill a fresh
        // construct as a repair, and the model would not find the attribute it
        // was sent to remove.
        let (_, decided) = answered(SuppressionVerdict::Denied);

        assert!(
            matches!(decided.outcome, Outcome::SuppressionAnswered { reopened: true, .. }),
            "{:?}",
            decided.outcome
        );
        let dispatched = decided.effects.iter().find_map(|effect| match effect {
            Decision::DispatchAttempt { stage, transformation, .. } => Some((*stage, transformation.checkout)),
            _ => None,
        });
        assert_eq!(dispatched, Some((StageId::Refine, digest(22))));
    }

    #[test]
    fn a_denial_spends_a_repair_roll_under_the_suppression_identity() {
        // Tripwire: without the roll a reviewer could deny the same member
        // forever and the member would never wedge — a person's "no" would be
        // the one verdict in the estate with no budget behind it.
        let (next, _) = answered(SuppressionVerdict::Denied);
        let bloom = *next.blooms.keys().next().expect("the bloom sealed");
        let progress = next.blooms[&bloom].progress.get(&workpiece("alpha")).copied().expect("the member re-opened");

        assert_eq!(progress.stage, StageId::Refine);
        assert_eq!(progress.repair_rolls, 1);
        assert!(progress.seen_verify_failures.contains(VerifyFailure::Suppress));
    }

    #[test]
    fn an_answer_naming_a_non_member_is_refused() {
        let (snapshot, bloom) = member_at_verify();

        let (_, decided) = step(
            &snapshot,
            &event(
                "answer-stranger",
                Fact::SuppressionDisposition {
                    bloom,
                    workpiece: workpiece("stranger"),
                    disposition: answer(SuppressionVerdict::Granted),
                },
            ),
        );

        assert!(
            matches!(decided.outcome, Outcome::SuppressionRejected(SuppressionDispositionError::NotAMember(_))),
            "{:?}",
            decided.outcome
        );
    }

    #[test]
    fn a_blank_operator_is_refused_rather_than_defaulted() {
        // A grant is cheap by design — read a line, paste a marker — so the one
        // thing the record must never lose is who gave it.
        let (snapshot, bloom) = member_at_verify();
        let mut anonymous = answer(SuppressionVerdict::Granted);
        anonymous.operator = "   ".to_string();

        let (_, decided) = step(
            &snapshot,
            &event(
                "answer-anonymous",
                Fact::SuppressionDisposition { bloom, workpiece: workpiece("alpha"), disposition: anonymous },
            ),
        );

        assert!(
            matches!(decided.outcome, Outcome::SuppressionRejected(SuppressionDispositionError::BlankOperator)),
            "{:?}",
            decided.outcome
        );
    }
}
