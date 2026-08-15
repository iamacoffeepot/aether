//! ADR-0189 — fold collisions dispatch Reconcile, rejoin the line, and
//! exhaust into a wedge with the conflict evidence attached. The journal
//! is the only state.

mod common;

use aether_bloomery::{CandidateRef, Decision, Evidence, EvidenceKind, Fact, Outcome, Snapshot, StageId};
use common::{claim, digest, draft, event, membership, step, workpiece};

fn conflict_evidence(checkpoint: u8, detail: u8) -> Evidence {
    Evidence { subject: digest(checkpoint), kind: EvidenceKind::FoldConflict, detail: digest(detail) }
}

fn attempt_evidence() -> Evidence {
    Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(80) }
}

/// Two members have verified claims and a captured candidate on the cursor.
/// The later one collides on the fold. Construct runs first so Reconcile
/// can tell a fold-time collision (has a candidate) from base assembly.
fn two_member_with_claims() -> (Snapshot, aether_bloomery::BloomId) {
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (mut snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    for (name, revision, tree, checkout) in [("alpha", 10, 20, 22), ("beta", 11, 21, 23)] {
        snapshot = step(
            &snapshot,
            &event(
                &format!("construct-{name}"),
                Fact::AttemptCompleted {
                    bloom,
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
            &event(&format!("integrate-{name}"), Fact::Integrate { bloom, claim: claim(name, revision, tree) }),
        )
        .0;
    }
    (snapshot, bloom)
}

// ADR-0189 — a FoldConflict revokes the later member's claim, moves it to
// Reconcile, and dispatches that stage against the folded checkpoint's head,
// not the sealed base. Catches the refusal-in-prose regression: no fact,
// no dispatch, a stalled bloom.
#[test]
fn a_fold_conflict_dispatches_reconcile_against_the_folded_checkpoint() {
    let (snapshot, bloom) = two_member_with_claims();
    let checkpoint = digest(30);
    let head = digest(31);
    let evidence = conflict_evidence(30, 90);

    let (after, decided) = step(
        &snapshot,
        &event(
            "fold-conflict-beta",
            Fact::FoldConflict { bloom, workpiece: workpiece("beta"), checkpoint, head, evidence: evidence.clone() },
        ),
    );

    assert!(
        matches!(&decided.outcome, Outcome::FoldConflictDispatched { workpiece, .. } if workpiece.0 == "beta"),
        "the later member is the one that reconciles: {:?}",
        decided.outcome,
    );
    assert!(
        decided.effects.iter().any(|effect| matches!(
            effect,
            Decision::RevokeResolution { workpiece, .. } if workpiece.0 == "beta"
        )),
        "the conflicted claim is revoked so the bloom cannot resolve on it",
    );
    let dispatch = decided.effects.iter().find_map(|effect| match effect {
        Decision::DispatchAttempt { workpiece, stage, transformation, .. } if workpiece.0 == "beta" => {
            Some((*stage, transformation.checkout))
        }
        _ => None,
    });
    assert_eq!(
        dispatch,
        Some((StageId::Reconcile, head)),
        "Reconcile checks out the folded checkpoint head, not the sealed base",
    );

    let record = after.blooms.get(&bloom).expect("the sealed bloom is still in the snapshot");
    assert!(!record.claims.contains_key(&workpiece("beta")), "the revoked claim is gone");
    assert!(record.claims.contains_key(&workpiece("alpha")), "the already-folded member keeps its claim");
    let progress = record.progress.get(&workpiece("beta")).expect("beta still has a progress cursor");
    assert_eq!(progress.stage, StageId::Reconcile);
    assert_eq!(progress.attempts, 1);
    assert_eq!(progress.fold_checkpoint, Some(head));
    assert_eq!(progress.fold_conflict_evidence, Some(evidence.detail));
}

// A passing Reconcile adopts the new candidate and returns to Verify — the
// ordinary line — so the reconciled tree faces the same gates as any candidate.
#[test]
fn a_passing_reconcile_rejoins_verify_with_the_new_candidate() {
    let (snapshot, bloom) = two_member_with_claims();
    let head = digest(31);
    let (snapshot, _) = step(
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

    let captured = CandidateRef { tree: digest(41), checkout: digest(42) };
    let (after, decided) = step(
        &snapshot,
        &event(
            "reconcile-pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("beta"),
                stage: StageId::Reconcile,
                passed: true,
                evidence: attempt_evidence(),
                candidate: Some(captured),
            },
        ),
    );

    match decided.outcome {
        Outcome::AttemptAdvanced { from, to, .. } => {
            assert_eq!(from, StageId::Reconcile);
            assert_eq!(to, StageId::Verify);
        }
        other => panic!("expected AttemptAdvanced onto Verify, got {other:?}"),
    }
    let dispatch = decided.effects.iter().find_map(|effect| match effect {
        Decision::DispatchAttempt { stage, transformation, .. } => Some((*stage, transformation.checkout)),
        _ => None,
    });
    assert_eq!(dispatch, Some((StageId::Verify, captured.checkout)), "Verify checks out the reconciled capture");

    let progress = after
        .blooms
        .get(&bloom)
        .expect("the sealed bloom is still in the snapshot")
        .progress
        .get(&workpiece("beta"))
        .expect("beta still has a progress cursor");
    assert_eq!(progress.stage, StageId::Verify);
    assert_eq!(progress.candidate, Some(captured));
    assert_eq!(
        progress.fold_checkpoint,
        Some(head),
        "the fold round outlives the stage: the reconciled candidate has not folded yet (#4952)",
    );
    assert_eq!(progress.fold_conflict_evidence, None, "the wedge attachment belongs to the stage that just passed");
}

// Exhausting Reconcile's budget wedges with the collision evidence, not the
// last attempt's — the paths that started the stage are what a later reader
// (and a grant) needs.
#[test]
fn exhausting_reconcile_wedges_with_the_conflict_evidence() {
    let (snapshot, bloom) = two_member_with_claims();
    let conflict = conflict_evidence(30, 90);
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "fold-conflict-beta",
            Fact::FoldConflict {
                bloom,
                workpiece: workpiece("beta"),
                checkpoint: digest(30),
                head: digest(31),
                evidence: conflict.clone(),
            },
        ),
    );

    let fail = |key: &str| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("beta"),
                stage: StageId::Reconcile,
                passed: false,
                evidence: attempt_evidence(),
                candidate: None,
            },
        )
    };

    let (after_retry, retried) = step(&snapshot, &fail("reconcile-fail-1"));
    assert!(matches!(retried.outcome, Outcome::AttemptRetried { stage: StageId::Reconcile, attempt: 2, .. }));

    let (after_wedge, wedged) = step(&after_retry, &fail("reconcile-fail-2"));
    assert!(matches!(wedged.outcome, Outcome::AttemptWedged { stage: StageId::Reconcile, .. }));
    assert!(
        !wedged.effects.iter().any(|effect| matches!(effect, Decision::DispatchAttempt { .. })),
        "a wedged member stops dispatching",
    );
    let wedge = after_wedge
        .blooms
        .get(&bloom)
        .expect("the sealed bloom is still in the snapshot")
        .wedged
        .get(&workpiece("beta"))
        .expect("the wedge is recorded");
    assert_eq!(wedge.stage, StageId::Reconcile);
    assert_eq!(wedge.evidence, conflict.detail, "the wedge attaches the collision evidence, not the last attempt");
}

// Journal replay is apply-only: the same facts produce the same snapshot
// without re-deciding. The reactor holds nothing the journal does not.
#[test]
fn a_replayed_journal_reproduces_the_fold_conflict_sequence() {
    let (mut live, bloom) = two_member_with_claims();
    let events = [
        event(
            "fold-conflict-beta",
            Fact::FoldConflict {
                bloom,
                workpiece: workpiece("beta"),
                checkpoint: digest(30),
                head: digest(31),
                evidence: conflict_evidence(30, 90),
            },
        ),
        event(
            "reconcile-pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("beta"),
                stage: StageId::Reconcile,
                passed: true,
                evidence: attempt_evidence(),
                candidate: Some(CandidateRef { tree: digest(41), checkout: digest(42) }),
            },
        ),
        event("integrate-beta-again", Fact::Integrate { bloom, claim: claim("beta", 11, 41) }),
    ];

    let mut recorded = Vec::new();
    for next in &events {
        let (snapshot, decisions) = step(&live, next);
        recorded.push((next.clone(), decisions));
        live = snapshot;
    }

    let mut replayed = two_member_with_claims().0;
    for (event, decisions) in &recorded {
        replayed = replayed.apply(event, decisions, &aether_bloomery::ResolvedConfigs::default());
    }

    assert_eq!(live, replayed, "apply-only replay rebuilds the live snapshot");
    let record = live.blooms.get(&bloom).expect("the sealed bloom is still in the snapshot");
    assert_eq!(record.claims.get(&workpiece("beta")).expect("the replaced claim is recorded").candidate, digest(41));
    assert!(
        recorded[2].1.effects.iter().any(|effect| matches!(effect, Decision::DispatchIntegration { .. })),
        "the replaced claim re-dispatches the fold",
    );
}

// ADR-0196 residual splice: a FoldConflict for a member that has not yet
// constructed is base assembly, not a reactor bug. Reconcile writes the
// spliced tree; a pass returns the member to Construct on that checkout.
#[test]
fn a_splice_conflict_on_a_dependent_dispatches_reconcile() {
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));

    let (after, decided) = step(
        &snapshot,
        &event(
            "splice-conflict-beta",
            Fact::FoldConflict {
                bloom,
                workpiece: workpiece("beta"),
                checkpoint: digest(30),
                head: digest(31),
                evidence: conflict_evidence(30, 90),
            },
        ),
    );

    assert!(
        matches!(&decided.outcome, Outcome::FoldConflictDispatched { workpiece, .. } if workpiece.0 == "beta"),
        "the dependent reconciles its base assembly: {:?}",
        decided.outcome,
    );
    assert!(
        !decided.effects.iter().any(|effect| matches!(effect, Decision::RevokeResolution { .. })),
        "there is no claim to revoke: the member has not constructed",
    );
    let dispatch = decided.effects.iter().find_map(|effect| match effect {
        Decision::DispatchAttempt { workpiece, stage, transformation, .. } if workpiece.0 == "beta" => {
            Some((*stage, transformation.checkout))
        }
        _ => None,
    });
    assert_eq!(dispatch, Some((StageId::Reconcile, digest(31))), "Reconcile checks out the assembled checkpoint");

    let progress = after
        .blooms
        .get(&bloom)
        .expect("the sealed bloom is still in the snapshot")
        .progress
        .get(&workpiece("beta"))
        .expect("beta entered the line at Reconcile");
    assert_eq!(progress.stage, StageId::Reconcile);
    assert_eq!(progress.candidate, None);
    assert_eq!(progress.fold_checkpoint, Some(digest(31)));
}

// A passing base-assembly Reconcile returns to Construct on the assembled
// capture, not to Verify — the capture is the spliced base, not the member's
// work. Catches routing the dependent into Verify against a tree it never
// authored.
#[test]
fn a_passing_base_assembly_reconcile_returns_to_construct() {
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "splice-conflict-beta",
            Fact::FoldConflict {
                bloom,
                workpiece: workpiece("beta"),
                checkpoint: digest(30),
                head: digest(31),
                evidence: conflict_evidence(30, 90),
            },
        ),
    );

    let captured = CandidateRef { tree: digest(41), checkout: digest(42) };
    let (after, decided) = step(
        &snapshot,
        &event(
            "reconcile-pass",
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece("beta"),
                stage: StageId::Reconcile,
                passed: true,
                evidence: attempt_evidence(),
                candidate: Some(captured),
            },
        ),
    );

    match decided.outcome {
        Outcome::AttemptAdvanced { from, to, .. } => {
            assert_eq!(from, StageId::Reconcile);
            assert_eq!(to, StageId::Construct);
        }
        other => panic!("expected AttemptAdvanced onto Construct, got {other:?}"),
    }
    let dispatch = decided.effects.iter().find_map(|effect| match effect {
        Decision::DispatchAttempt { stage, transformation, .. } => Some((*stage, transformation.checkout)),
        _ => None,
    });
    assert_eq!(
        dispatch,
        Some((StageId::Construct, captured.checkout)),
        "Construct checks out the assembled capture, not the sealed base",
    );

    let progress = after
        .blooms
        .get(&bloom)
        .expect("the sealed bloom is still in the snapshot")
        .progress
        .get(&workpiece("beta"))
        .expect("beta still has a progress cursor");
    assert_eq!(progress.stage, StageId::Construct);
    assert_eq!(progress.candidate, Some(captured), "Construct checks out the assembled capture");
    assert_eq!(
        progress.fold_checkpoint,
        Some(digest(31)),
        "the collision head stays so a standing re-collision still wedges",
    );
}
