//! Fold collisions become a journaled fact that dispatches `Reconcile` (ADR-0189).

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate, wedged};
use super::{BloomStatus, Decision, Decisions, FoldConflictError, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, VerifyFailureSet};

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
    // is nothing to revoke, and Reconcile assembles the base instead.
    let claimed = record.claims.contains_key(workpiece);
    let cursor = record.progress.get(workpiece).copied();
    let candidate = cursor.and_then(|progress| progress.candidate);
    let mut effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    if claimed {
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
