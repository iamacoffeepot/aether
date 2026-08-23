//! A declining lane asked for the surface its work requires (ADR-0207).
//!
//! The request half: the lane returned machine-readable paths, and the reducer
//! parks the member awaiting a surface amendment. Nothing about the member's
//! work moves — no attempt, no repair roll, no cursor, no candidate — because
//! the remedy is a person widening a boundary, and another lap would reproduce
//! the same refusal verbatim.
//!
//! Empty effects beyond recording the evidence: the snapshot folds the request
//! straight off [`Fact::SurfaceRequested`](crate::Fact::SurfaceRequested), the
//! way a fold refusal is folded from its own fact, so no new
//! [`Decision`] enters the wire-frozen decisions graph.

use alloc::vec;

use alloc::string::String;

use super::attempt::{DispatchTargets, SealedLine, move_effects_with_candidate};
use super::{BloomStatus, Decision, Decisions, Outcome, Snapshot, SurfaceRequestedError};
use crate::digest::{Digest, digest_of};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, SurfaceRequest};

/// The stages whose lane runs `construct.implement` and can therefore decline
/// for want of surface. `LaneGates::of` keys `is_construct` on the command,
/// which `StageCatalog` maps from all three, so a declining repair lap asks the
/// same way a first construct does.
const fn is_construct_family(stage: StageId) -> bool {
    matches!(stage, StageId::Construct | StageId::Refine | StageId::Reconcile)
}

/// Reduce a declining lane's surface request against a snapshot.
///
/// The refusal ladder mirrors
/// [`reduce_attempt_completed`](super::reduce_attempt_completed)'s: an unknown
/// or non-`Sealed` bloom, a workpiece that is not a member, a member with no
/// cursor, a cursor whose stage is not `stage`, a stage outside the construct
/// family, and a request naming a revision other than the member's sealed one.
pub(super) fn reduce_surface_requested(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    evidence: &Evidence,
    request: &SurfaceRequest,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::SurfaceRequestRejected(SurfaceRequestedError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::SurfaceRequestRejected(SurfaceRequestedError::UnknownOrInactiveBloom));
    }
    // The composition workpiece is routed out here rather than by a separate
    // arm: it is a subject like a member but declares no surface, so it can
    // never be the thing a surface amendment widens.
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::SurfaceRequestRejected(SurfaceRequestedError::NotAMember(
            workpiece.clone(),
        )));
    };
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::SurfaceRequestRejected(SurfaceRequestedError::NotDispatched(
            workpiece.clone(),
        )));
    };
    if cursor.stage != stage {
        return Decisions::rejected(Outcome::SurfaceRequestRejected(SurfaceRequestedError::StageMismatch {
            expected: cursor.stage,
            got: stage,
        }));
    }
    if !is_construct_family(stage) {
        return Decisions::rejected(Outcome::SurfaceRequestRejected(SurfaceRequestedError::NotAConstructFamilyStage(
            stage,
        )));
    }
    if request.scope_revision != member.scope_revision {
        return Decisions::rejected(Outcome::SurfaceRequestRejected(SurfaceRequestedError::RevisionMismatch {
            expected: member.scope_revision,
            got: request.scope_revision,
        }));
    }

    let requests = snapshot.awaiting_surface(bloom, workpiece).map_or(0, |awaiting| awaiting.requests) + 1;

    Decisions {
        outcome: Outcome::SurfaceRequested {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            stage,
            request: digest_of(request),
            requests,
        },
        effects: vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }],
    }
}

/// Reduce a granted surface amendment (ADR-0207).
///
/// The mirror of [`reduce_surface_requested`]: the same ladder, and then the
/// opposite outcome — the member's pin moves to the successor revision and its
/// lane re-enters at the stage that declined, rather than parking on a person.
///
/// The grant costs no attempt. The lap it buys is the one the declining lap
/// should have been able to run, and charging for the estate's own
/// under-scoping would spend a member's budget on a boundary it did not draw.
pub(super) fn reduce_surface_granted(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    stage: StageId,
    revision: Digest,
    added: &[String],
    evidence: &Evidence,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom).filter(|record| record.status == BloomStatus::Sealed) else {
        return Decisions::rejected(Outcome::SurfaceGrantRejected(SurfaceRequestedError::UnknownOrInactiveBloom));
    };
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::SurfaceGrantRejected(SurfaceRequestedError::NotAMember(
            workpiece.clone(),
        )));
    };
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::SurfaceGrantRejected(SurfaceRequestedError::NotDispatched(
            workpiece.clone(),
        )));
    };
    if cursor.stage != stage {
        return Decisions::rejected(Outcome::SurfaceGrantRejected(SurfaceRequestedError::StageMismatch {
            expected: cursor.stage,
            got: stage,
        }));
    }
    if !is_construct_family(stage) {
        return Decisions::rejected(Outcome::SurfaceGrantRejected(SurfaceRequestedError::NotAConstructFamilyStage(
            stage,
        )));
    }

    let targets = DispatchTargets {
        subject: cursor.candidate.map_or(revision, |candidate| candidate.tree),
        checkout: cursor.candidate.map_or_else(|| record.spec.base(), |candidate| candidate.checkout),
    };
    // The pin folds straight off the fact, the way this member's park already
    // does, so no new decision enters the wire-frozen graph for it.
    let mut effects = vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];
    effects.extend(move_effects_with_candidate(
        *bloom,
        workpiece,
        revision,
        cursor,
        targets,
        cursor.candidate.map(|candidate| candidate.tree),
        SealedLine::of(record, member),
    ));

    Decisions {
        outcome: Outcome::SurfaceGranted {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            revision,
            added: added.to_vec(),
        },
        effects,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use alloc::string::{String, ToString as _};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{reduce_surface_granted, reduce_surface_requested};
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, StageId, WorkpieceId};
    use crate::reduce::{Decision, Event, Fact, Outcome, Snapshot, SurfaceRequestedError, reduce};
    use crate::testing::{draft, membership};
    use crate::values::{Evidence, EvidenceKind, ResolvedConfigs, SpendWindow, SurfaceRequest};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    /// A sealed single-member bloom whose member sits at its entry cursor.
    fn sealed() -> (Snapshot, BloomId, WorkpieceId, Digest) {
        let spec = draft(0, vec![membership("wp-0", 1)]).seal();
        let bloom = spec.id();
        let workpiece = spec.members()[0].workpiece.clone();
        let scope_revision = spec.members()[0].scope_revision;
        let snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
        let seal = Event { idempotency_key: IdempotencyKey("seal".into()), fact: Fact::Seal(spec) };
        let snapshot = snapshot.apply(
            &seal,
            &reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default()),
            &ResolvedConfigs::default(),
        );
        (snapshot, bloom, workpiece, scope_revision)
    }

    fn request(scope_revision: Digest) -> SurfaceRequest {
        SurfaceRequest::normalize(
            scope_revision,
            &Vec::new(),
            "the caller this refine must update lives outside the sealed surface",
            vec![("crates/example-b/src/lib.rs".to_string(), "the caller".to_string())],
        )
        .expect("a literal path normalizes")
    }

    fn evidence(subject: Digest) -> Evidence {
        Evidence { subject, kind: EvidenceKind::ConstructDeclined, detail: digest(9) }
    }

    #[test]
    fn a_granted_request_moves_the_pin_and_puts_the_member_back_on_the_line() {
        // Tripwire: the whole point of the auto tier. A delta the owner's policy
        // already marked as needing nobody must not stop the estate — the park
        // it would otherwise take costs a person's attention for a decision the
        // policy has already made, which is what left a member sitting for three
        // hours.
        let (snapshot, bloom, workpiece, scope_revision) = sealed();
        let granted = digest(0x9A);
        let before = snapshot.blooms[&bloom].progress[&workpiece];

        let decisions = reduce_surface_granted(
            &snapshot,
            &bloom,
            &workpiece,
            StageId::Construct,
            granted,
            &[String::from("xtask/**")],
            &evidence(scope_revision),
        );

        let Outcome::SurfaceGranted { revision, added, .. } = &decisions.outcome else {
            panic!("an auto-tier delta is granted: {:?}", decisions.outcome);
        };
        assert_eq!(*revision, granted);
        assert_eq!(added, &[String::from("xtask/**")]);

        let dispatched = decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::DispatchAttempt { workpiece, stage, scope_revision, .. } => {
                    Some((workpiece.clone(), *stage, *scope_revision))
                }
                _ => None,
            })
            .expect("a grant puts the member back on the line");
        assert_eq!(dispatched.0, workpiece);
        assert_eq!(dispatched.1, StageId::Construct, "it re-enters at the stage that declined");
        assert_eq!(dispatched.2, granted, "and it dispatches under the widened revision, not the sealed one");

        let advanced = decisions
            .effects
            .iter()
            .find_map(|effect| match effect {
                Decision::AdvanceStage { progress, .. } => Some(*progress),
                _ => None,
            })
            .expect("the cursor moves with the dispatch");
        assert_eq!(
            advanced.attempts, before.attempts,
            "the lap a grant buys is the one the declining lap should have run, so it costs no attempt",
        );

        // And the pin is durable: the next dispatch reads the granted revision
        // off the record rather than the membership's sealed one.
        let event = Event {
            idempotency_key: IdempotencyKey("granted".into()),
            fact: Fact::SurfaceGranted {
                bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Construct,
                revision: granted,
                added: vec![String::from("xtask/**")],
                evidence: evidence(scope_revision),
            },
        };
        let after = snapshot.apply(&event, &decisions, &ResolvedConfigs::default());
        let record = &after.blooms[&bloom];
        let member = record.spec.members().iter().find(|member| member.workpiece == workpiece).unwrap();
        assert_eq!(record.scope_revision_of(member), granted);
        assert!(after.awaiting_surface(&bloom, &workpiece).is_none(), "a granted member is not parked");
    }

    #[test]
    fn a_request_at_the_cursor_parks_without_spending_the_members_budget() {
        // The whole point of parking rather than failing: a member that asked
        // for surface has produced no defect, so another attempt is not owed
        // and a repair roll is not spent.
        let (snapshot, bloom, workpiece, scope_revision) = sealed();
        let stage = snapshot.blooms[&bloom].progress[&workpiece].stage;
        let before = snapshot.blooms[&bloom].progress[&workpiece];

        let decided = reduce_surface_requested(
            &snapshot,
            &bloom,
            &workpiece,
            stage,
            &evidence(scope_revision),
            &request(scope_revision),
        );
        assert!(matches!(decided.outcome, Outcome::SurfaceRequested { requests: 1, .. }));

        let event = Event {
            idempotency_key: IdempotencyKey("surface".into()),
            fact: Fact::SurfaceRequested {
                bloom,
                workpiece: workpiece.clone(),
                stage,
                evidence: evidence(scope_revision),
                request: request(scope_revision),
            },
        };
        let after = snapshot.apply(&event, &decided, &ResolvedConfigs::default());
        let cursor = after.blooms[&bloom].progress[&workpiece];

        assert_eq!(cursor.attempts, before.attempts, "a park spends no attempt");
        assert_eq!(cursor.repair_rolls, before.repair_rolls, "a park spends no repair roll");
        assert_eq!(cursor.stage, before.stage, "a park does not move the cursor");
        assert_eq!(after.awaiting_surface(&bloom, &workpiece).unwrap().request.paths.len(), 1);
    }

    #[test]
    fn a_request_naming_another_revision_is_refused() {
        // The binding rule evidence follows: a request never widens a revision
        // it does not name, or a stale lane could amend the surface a newer
        // supersede sealed.
        let (snapshot, bloom, workpiece, scope_revision) = sealed();
        let stage = snapshot.blooms[&bloom].progress[&workpiece].stage;

        let decided = reduce_surface_requested(
            &snapshot,
            &bloom,
            &workpiece,
            stage,
            &evidence(scope_revision),
            &request(digest(0xAB)),
        );

        assert!(matches!(
            decided.outcome,
            Outcome::SurfaceRequestRejected(SurfaceRequestedError::RevisionMismatch { .. })
        ));
        assert!(decided.effects.is_empty(), "a refused request records nothing");
    }

    #[test]
    fn a_request_for_a_stage_the_member_has_left_is_refused() {
        let (snapshot, bloom, workpiece, scope_revision) = sealed();

        let decided = reduce_surface_requested(
            &snapshot,
            &bloom,
            &workpiece,
            StageId::Verify,
            &evidence(scope_revision),
            &request(scope_revision),
        );

        assert!(matches!(
            decided.outcome,
            Outcome::SurfaceRequestRejected(SurfaceRequestedError::StageMismatch { .. })
        ));
    }

    #[test]
    fn a_second_request_counts_up_rather_than_resetting() {
        // ADR-0207 budgets amendments per bloom, so the count has to survive a
        // lane restating its whole need on a later lap.
        let (snapshot, bloom, workpiece, scope_revision) = sealed();
        let stage = snapshot.blooms[&bloom].progress[&workpiece].stage;
        let fact = |key: &str| Event {
            idempotency_key: IdempotencyKey(key.into()),
            fact: Fact::SurfaceRequested {
                bloom,
                workpiece: workpiece.clone(),
                stage,
                evidence: evidence(scope_revision),
                request: request(scope_revision),
            },
        };

        let first = fact("surface-1");
        let snapshot = snapshot.apply(
            &first,
            &reduce_surface_requested(
                &snapshot,
                &bloom,
                &workpiece,
                stage,
                &evidence(scope_revision),
                &request(scope_revision),
            ),
            &ResolvedConfigs::default(),
        );
        let second = reduce_surface_requested(
            &snapshot,
            &bloom,
            &workpiece,
            stage,
            &evidence(scope_revision),
            &request(scope_revision),
        );
        assert!(matches!(second.outcome, Outcome::SurfaceRequested { requests: 2, .. }));

        let snapshot = snapshot.apply(&fact("surface-2"), &second, &ResolvedConfigs::default());
        assert_eq!(snapshot.awaiting_surface(&bloom, &workpiece).unwrap().requests, 2);
    }
}
