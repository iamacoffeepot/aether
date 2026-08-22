//! Fold collisions become a journaled fact that dispatches `Reconcile` (ADR-0189).

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate, wedged};
use super::{BloomStatus, Decision, Decisions, FoldConflictError, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{CandidateRef, Evidence, VerifyFailureSet};

/// Reduce a cross-member fold collision ([`Fact::FoldConflict`](crate::Fact::FoldConflict)).
///
/// The integrate reactor admits the collision instead of refusing it in prose.
/// A member that already carries a claim is being folded: this arm revokes it —
/// the bloom can no longer resolve on the conflicted candidate — moves the
/// cursor to `Reconcile`, and dispatches that stage against the folded
/// checkpoint. A member with no claim is a residual splice collision on its
/// construct base (ADR-0196): there is nothing to revoke, and Reconcile
/// assembles the base so Construct can follow. A passing fold-time Reconcile
/// rejoins at `Verify`; a passing base-assembly Reconcile returns to
/// `Construct`. Exhausting the catalog's Reconcile budget wedges with this
/// fact's evidence attached.
///
/// What the collision costs the member is decided by its provenance (#4952),
/// and the marker it is decided on is `fold_checkpoint` rather than `attempts`.
/// `attempts` is a per-*stage* cursor that resets at every advance, and a
/// re-collision necessarily arrives after the member advanced Reconcile →
/// Verify → claim, so by construction it always reads one and can never
/// accumulate across a round. `fold_checkpoint` is the durable marker: it names
/// the head this member was sent to reconcile onto, and it now outlives the
/// stage. So a collision naming a head the member has already reconciled onto is
/// the fold standing exactly where it stood when the lane was handed it, and
/// what came back still does not land — the member's own inability to reproduce
/// its intent, which is what ADR-0189 §5 wedges on. A head that differs is a
/// sibling's reconciled candidate folding underneath this one: the member's diff
/// never changed and it was never asked about this tree, so it opens a fresh
/// round however many siblings ripple through. The catalog's Reconcile
/// `retry_budget` is untouched and still governs a lane that fails its own gate
/// inside a round.
///
/// The cascade is bounded by rounds rather than by pairs, because a round that
/// moves the checkpoint has folded a member — so the conflicted set strictly
/// shrinks — and a round that moves nothing wedges what is left.
pub(super) fn reduce_fold_conflict(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    checkpoint: Digest,
    head: Digest,
    evidence: &Evidence,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::FoldConflictRejected(FoldConflictError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::FoldConflictRejected(FoldConflictError::UnknownOrInactiveBloom));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::FoldConflictRejected(FoldConflictError::NotAMember(workpiece.clone())));
    };
    // Evidence attests to the folded tree it collided with — the same
    // "no evidence validates a digest it does not name" rule every other
    // admission door runs.
    if !evidence.validates(&checkpoint) {
        return Decisions::rejected(Outcome::FoldConflictRejected(FoldConflictError::EvidenceNotBound {
            expected: checkpoint,
            got: evidence.subject,
        }));
    }

    // A member already carrying a claim is being folded: revoke so the bloom
    // cannot resolve on the conflicted candidate. A member with no claim is
    // a residual splice collision on its construct base (ADR-0196) — there
    // is nothing to revoke, and Reconcile assembles the base instead. The
    // assembly bit is recorded on the cursor here: once the claim is gone,
    // completion cannot tell the two cases apart.
    let claimed = record.claims.contains_key(workpiece);
    let cursor = record.progress.get(workpiece).copied();
    let mut candidate = cursor.and_then(|progress| progress.candidate);
    let assembling = !claimed && candidate.is_none();
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    if claimed {
        // An inherited claim has no cursor; the claim is the only record of
        // the tree the member resolved on. Read it before the revoke effect
        // is applied — the reducer is pure over `snapshot`, so the claim is
        // still in `record`. A member with a cursor keeps that candidate.
        if candidate.is_none()
            && let Some(claim) = record.claims.get(workpiece)
        {
            candidate = Some(CandidateRef { tree: claim.candidate, checkout: head });
        }
        effects.push(Decision::RevokeResolution { bloom: *bloom, workpiece: workpiece.clone() });
    }
    // A second collision with a head the member has already reconciled onto
    // stops here with the collision evidence attached (ADR-0189 §5), rather than
    // buying another lane against the very tree the last one was handed. The
    // revoked claim stands: a candidate that cannot fold must not complete the
    // claim set and re-dispatch this fold.
    if cursor.is_some_and(|progress| progress.fold_checkpoint == Some(head)) {
        return wedged(*bloom, workpiece, StageId::Reconcile, evidence, effects);
    }

    let progress = StageProgress {
        stage: StageId::Reconcile,
        attempts: 1,
        candidate,
        repair_rolls: cursor.map_or(0, |progress| progress.repair_rolls),
        seen_verify_failures: cursor.map_or(VerifyFailureSet::EMPTY, |progress| progress.seen_verify_failures),
        fold_checkpoint: Some(head),
        fold_conflict_evidence: Some(evidence.detail),
        reconcile_assembles_base: assembling,
    };
    let subject = candidate.map_or(member.scope_revision, |current| current.tree);

    effects.extend(move_effects_with_candidate(
        *bloom,
        workpiece,
        member.scope_revision,
        progress,
        DispatchTargets { subject, checkout: head },
        candidate.map(|current| current.tree),
        SealedLine::of(record, member),
    ));

    Decisions { outcome: Outcome::FoldConflictDispatched { bloom: *bloom, workpiece: workpiece.clone() }, effects }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reduce::{Decision, Fact, Outcome};
    use crate::testing::{claim, digest, draft, event, membership, observing, step, workpiece};
    use crate::values::EvidenceKind;

    fn conflict_evidence(checkpoint: u8, detail: u8) -> Evidence {
        Evidence { subject: digest(checkpoint), kind: EvidenceKind::FoldConflict, detail: digest(detail) }
    }

    fn attempt_evidence() -> Evidence {
        Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(80) }
    }

    /// A successor that adopted a predecessor's claim and never dispatched the
    /// member — claim present, no cursor. The supersede-adoption shape the
    /// misroute was observed on.
    fn inherit_claimed_successor() -> (Snapshot, BloomId, Digest) {
        let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
        let predecessor = spec.id();
        let (mut snapshot, _) =
            step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
        for (name, revision, tree, checkout) in [("alpha", 10, 20, 22), ("beta", 11, 21, 23)] {
            snapshot = step(
                &snapshot,
                &event(
                    &format!("construct-{name}"),
                    Fact::AttemptCompleted {
                        bloom: predecessor,
                        workpiece: workpiece(name),
                        stage: StageId::Construct,
                        passed: true,
                        evidence: attempt_evidence(),
                        candidate: Some(CandidateRef { tree: digest(tree), checkout: digest(checkout) }),
                    },
                ),
            )
            .0;
            snapshot = step(
                &snapshot,
                &event(
                    &format!("integrate-{name}"),
                    Fact::Integrate { bloom: predecessor, claim: claim(name, revision, tree) },
                ),
            )
            .0;
        }
        let snapshot = observing(&snapshot, 2).with_green_base(digest(2));
        let successor_spec = draft(2, vec![membership("alpha", 10), membership("beta", 11)]).seal();
        let successor = successor_spec.id();
        let (snapshot, decided) =
            step(&snapshot, &event("sup", Fact::Supersede { predecessor, successor: successor_spec }));
        assert!(matches!(decided.outcome, Outcome::Superseded { .. }), "the successor seals: {:?}", decided.outcome);
        let record = snapshot.blooms.get(&successor).expect("the successor is in the snapshot");
        assert!(record.claims.contains_key(&workpiece("beta")), "beta arrived by InheritClaim");
        assert!(!record.progress.contains_key(&workpiece("beta")), "an inherited claim has no cursor in this bloom");
        (snapshot, successor, digest(21))
    }

    // The plausible bug: an inherited claim has no cursor, so FoldConflict
    // reads candidate as None and Reconcile binds the scope revision instead
    // of the tree the member already resolved on.
    #[test]
    fn a_fold_conflict_on_an_inherited_claim_seeds_the_reconcile_from_its_claimed_candidate() {
        let (snapshot, bloom, claimed_tree) = inherit_claimed_successor();
        let head = digest(31);
        let scope_revision = digest(11);

        let (_, decided) = step(
            &snapshot,
            &event(
                "fold-conflict-beta",
                Fact::FoldConflict {
                    bloom,
                    workpiece: workpiece("beta"),
                    checkpoint: digest(30),
                    head,
                    evidence: conflict_evidence(30, 90),
                },
            ),
        );

        let dispatch = decided.effects.iter().find_map(|effect| match effect {
            Decision::DispatchAttempt { workpiece, stage, transformation, .. } if workpiece.0 == "beta" => {
                Some((*stage, transformation.inputs[0], transformation.checkout))
            }
            _ => None,
        });
        assert_eq!(
            dispatch,
            Some((StageId::Reconcile, claimed_tree, head)),
            "Reconcile binds the claimed candidate tree, not the scope revision",
        );
        assert_ne!(claimed_tree, scope_revision, "the claimed tree and the scope revision are distinct axes");
    }
}
