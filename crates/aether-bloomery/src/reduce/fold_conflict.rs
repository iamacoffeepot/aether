//! Fold collisions become a journaled fact that dispatches `Reconcile` (ADR-0189).

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate};
use super::{BloomStatus, Decision, Decisions, FoldConflictError, Outcome, Snapshot, StageProgress};
use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, VerifyFailureSet};

/// Reduce a cross-member fold collision ([`Fact::FoldConflict`](crate::Fact::FoldConflict)).
///
/// The integrate reactor admits the collision instead of refusing it in prose.
/// This arm revokes the later member's claim — the bloom can no longer resolve
/// on the conflicted candidate — moves the cursor to `Reconcile`, and dispatches
/// that stage against the folded checkpoint. A passing Reconcile rejoins the
/// ordinary line at `Verify`; exhausting the catalog's Reconcile budget wedges
/// with this fact's evidence attached.
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
    // Only a member that was being folded — one that already carries a
    // resolution — can collide. A FoldConflict for a member that never
    // verified is a reactor bug, not a collision to reconcile.
    if !record.claims.contains_key(workpiece) {
        return Decisions::rejected(Outcome::FoldConflictRejected(FoldConflictError::NotIntegrated(workpiece.clone())));
    }
    // Evidence attests to the folded tree it collided with — the same
    // "no evidence validates a digest it does not name" rule every other
    // admission door runs.
    if !evidence.validates(&checkpoint) {
        return Decisions::rejected(Outcome::FoldConflictRejected(FoldConflictError::EvidenceNotBound {
            expected: checkpoint,
            got: evidence.subject,
        }));
    }

    let cursor = record.progress.get(workpiece).copied();
    let candidate = cursor.and_then(|progress| progress.candidate);
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

    let mut effects = alloc::vec![
        Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() },
        Decision::RevokeResolution { bloom: *bloom, workpiece: workpiece.clone() },
    ];
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
