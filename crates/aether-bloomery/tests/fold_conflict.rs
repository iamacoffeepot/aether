//! ADR-0189 — fold collisions dispatch Reconcile, rejoin the line, and
//! exhaust into a wedge with the conflict evidence attached. The journal
//! is the only state.

mod common;

use aether_bloomery::{
    AttemptCompletedError, BloomId, CandidateRef, CompositionParents, Decision, Decisions, Event, Evidence,
    EvidenceKind, Fact, Outcome, Snapshot, StageId, VerifyFailure, VerifyFailureSet, Withdrawal, WithdrawalCause,
    WorkpieceId,
};
use common::{claim, compiled_resolved, digest, draft, event, membership, step, workpiece};

fn conflict_evidence(checkpoint: u8, detail: u8) -> Evidence {
    Evidence { subject: digest(checkpoint), kind: EvidenceKind::FoldConflict, detail: digest(detail) }
}

fn attempt_evidence() -> Evidence {
    Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(80) }
}

/// Two members have verified claims and a captured candidate on the cursor.
/// The later one collides on the fold. Construct runs first so Reconcile
/// can tell a fold-time collision (has a candidate) from base assembly.
fn two_member_with_claims() -> (Snapshot, BloomId) {
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (mut snapshot, _) =
        step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
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

fn fail_reconcile(bloom: BloomId, name: &str, key: &str) -> Event {
    event(
        key,
        Fact::AttemptCompleted {
            bloom,
            workpiece: workpiece(name),
            stage: StageId::Reconcile,
            passed: false,
            evidence: attempt_evidence(),
            candidate: None,
        },
    )
}

fn pass_reconcile(bloom: BloomId, name: &str, key: &str, captured: CandidateRef) -> Event {
    event(
        key,
        Fact::AttemptCompleted {
            bloom,
            workpiece: workpiece(name),
            stage: StageId::Reconcile,
            passed: true,
            evidence: attempt_evidence(),
            candidate: Some(captured),
        },
    )
}

fn grant_reconcile(bloom: BloomId, name: &str, attempts: u32) -> Event {
    event("grant", Fact::GrantAttempts { bloom, workpiece: workpiece(name), stage: StageId::Reconcile, attempts })
}

fn exhaust_reconcile(snapshot: &Snapshot, bloom: BloomId, name: &str) -> Snapshot {
    let (retried, decided) = step(snapshot, &fail_reconcile(bloom, name, "reconcile-fail-1"));
    assert!(matches!(decided.outcome, Outcome::AttemptRetried { stage: StageId::Reconcile, attempt: 2, .. }));
    let (wedged, decided) = step(&retried, &fail_reconcile(bloom, name, "reconcile-fail-2"));
    assert!(matches!(decided.outcome, Outcome::AttemptWedged { stage: StageId::Reconcile, .. }));
    wedged
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
        replayed = replayed.apply(event, decisions, &compiled_resolved());
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
    let (snapshot, _) = step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));

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
    let (snapshot, _) = step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
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

// #5558 — granting more Reconcile attempts after a dependent's base-assembly
// budget is spent must not drop the assembly bit. The plausible bug: the grant
// writes `reconcile_assembles_base: false`, so the next pass routes to Verify
// and skips the dependent's Construct.
#[test]
fn a_grant_after_exhausted_base_assembly_still_returns_to_construct() {
    let spec = draft(1, vec![membership("alpha", 10), membership("beta", 11)]).seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
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
    let snapshot = exhaust_reconcile(&snapshot, bloom, "beta");

    let (granted, decided) = step(&snapshot, &grant_reconcile(bloom, "beta", 2));
    assert!(
        matches!(&decided.outcome, Outcome::AttemptsGranted { resumes_at: StageId::Reconcile, attempts: 2, .. }),
        "the grant resumes the Reconcile lap: {:?}",
        decided.outcome,
    );

    let captured = CandidateRef { tree: digest(41), checkout: digest(42) };
    let (_, decided) = step(&granted, &pass_reconcile(bloom, "beta", "reconcile-pass", captured));
    match decided.outcome {
        Outcome::AttemptAdvanced { from, to, .. } => {
            assert_eq!(from, StageId::Reconcile);
            assert_eq!(to, StageId::Construct);
        }
        other => panic!("expected AttemptAdvanced onto Construct after the grant, got {other:?}"),
    }
    let dispatch = decided.effects.iter().find_map(|effect| match effect {
        Decision::DispatchAttempt { stage, transformation, .. } => Some((*stage, transformation.checkout)),
        _ => None,
    });
    assert_eq!(
        dispatch,
        Some((StageId::Construct, captured.checkout)),
        "Construct checks out the assembled capture after the grant",
    );
}

// #5558 — the grant must copy the cursor bit, not force assembly mode on every
// Reconcile resume. A fold-time Reconcile that already had a candidate still
// rejoins Verify after a grant; forcing the bit true would send it back through
// Construct and re-implement work it already authored.
#[test]
fn a_grant_after_exhausted_fold_reconcile_still_returns_to_verify() {
    let (snapshot, bloom) = two_member_with_claims();
    let (snapshot, _) = step(
        &snapshot,
        &event(
            "fold-conflict-beta",
            Fact::FoldConflict {
                bloom,
                workpiece: workpiece("beta"),
                checkpoint: digest(30),
                head: digest(31),
                evidence: conflict_evidence(30, 90),
            },
        ),
    );
    let snapshot = exhaust_reconcile(&snapshot, bloom, "beta");

    let (granted, decided) = step(&snapshot, &grant_reconcile(bloom, "beta", 2));
    assert!(
        matches!(&decided.outcome, Outcome::AttemptsGranted { resumes_at: StageId::Reconcile, attempts: 2, .. }),
        "the grant resumes the Reconcile lap: {:?}",
        decided.outcome,
    );

    let captured = CandidateRef { tree: digest(41), checkout: digest(42) };
    let (_, decided) = step(&granted, &pass_reconcile(bloom, "beta", "reconcile-pass", captured));
    match decided.outcome {
        Outcome::AttemptAdvanced { from, to, .. } => {
            assert_eq!(from, StageId::Reconcile);
            assert_eq!(to, StageId::Verify);
        }
        other => panic!("expected AttemptAdvanced onto Verify after the grant, got {other:?}"),
    }
}

fn collision_parents() -> CompositionParents {
    CompositionParents {
        parents: vec![workpiece("alpha"), workpiece("beta")],
        paths: vec!["xtask/src/transform/verify/mod.rs".into()],
        bound: vec!["xtask/**".into()],
    }
}

fn collision_subject() -> WorkpieceId {
    WorkpieceId::composition_of(&[workpiece("alpha"), workpiece("beta")])
}

fn construct_to_verify(snapshot: &Snapshot, bloom: BloomId, name: &str, tree: u8, checkout: u8) -> Snapshot {
    step(
        snapshot,
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
    .0
}

fn members_at_verify(names: &[&str]) -> (Snapshot, BloomId) {
    let spec = draft(
        1,
        names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                membership(name, u8::try_from(10 + index).expect("fixture membership count fits in a u8 seed"))
            })
            .collect(),
    )
    .seal();
    let bloom = spec.id();
    let (mut snapshot, _) =
        step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
    for (offset, name) in names.iter().enumerate() {
        if *name == "alpha" || *name == "beta" {
            continue;
        }
        let tree = u8::try_from(20 + offset).expect("fixture member offset fits in a u8 tree seed");
        snapshot = construct_to_verify(&snapshot, bloom, name, tree, tree.wrapping_add(10));
    }
    (snapshot, bloom)
}

fn narrow(snapshot: &Snapshot, bloom: BloomId, key: &str, verified: &str, tree: u8, head: u8) -> (Snapshot, Decisions) {
    step(
        snapshot,
        &event(
            key,
            Fact::CompositionNarrowed {
                bloom,
                verified: workpiece(verified),
                tree: digest(tree),
                head: digest(head),
                evidence: Evidence {
                    subject: digest(tree),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(50),
                },
                attribution: collision_parents(),
            },
        ),
    )
}

fn pass_narrowed_repair(snapshot: &Snapshot, bloom: BloomId, key: &str, tree: u8, head: u8) -> (Snapshot, Decisions) {
    step(
        snapshot,
        &event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: collision_subject(),
                stage: StageId::Refine,
                passed: true,
                evidence: Evidence {
                    subject: digest(tree),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(57),
                },
                candidate: Some(CandidateRef { tree: digest(tree), checkout: digest(head) }),
            },
        ),
    )
}

fn recorded_waiters(snapshot: &Snapshot, bloom: &BloomId) -> Vec<WorkpieceId> {
    snapshot
        .narrowed_compositions_of(bloom)
        .find(|(id, _)| *id == &collision_subject())
        .map(|(_, narrowed)| narrowed.waiters.clone())
        .unwrap_or_default()
}

fn verify_targets(decisions: &Decisions) -> Vec<WorkpieceId> {
    decisions
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Decision::DispatchAttempt { workpiece, stage: StageId::Verify, .. } => Some(workpiece.clone()),
            _ => None,
        })
        .collect()
}

// Tripwire: two independent Verifies can attribute the same composition on the
// same refused tree. Same-tree dedup used to drop the second waiter, so only
// one member resumed after the repair and the other sat holding a refusal
// about a tree that no longer existed.
#[test]
fn two_same_tree_waiters_both_reverify_after_repair() {
    let (snapshot, bloom) = members_at_verify(&["alpha", "beta", "gamma", "delta"]);
    let (snapshot, first) = narrow(&snapshot, bloom, "narrow-gamma", "gamma", 40, 41);
    assert!(matches!(first.outcome, Outcome::CompositionNarrowed { .. }), "{:?}", first.outcome);
    let (snapshot, second) = narrow(&snapshot, bloom, "narrow-delta", "delta", 40, 41);
    assert!(
        matches!(second.outcome, Outcome::CompositionRepairAlreadyInFlight { .. }),
        "the second same-tree attribution buys no second lane: {:?}",
        second.outcome,
    );
    let (snapshot, again) = narrow(&snapshot, bloom, "narrow-gamma-again", "gamma", 40, 41);
    assert!(matches!(again.outcome, Outcome::CompositionRepairAlreadyInFlight { .. }), "{:?}", again.outcome);
    assert_eq!(
        recorded_waiters(&snapshot, &bloom),
        vec![workpiece("gamma"), workpiece("delta")],
        "dedup keeps both waiters and does not record gamma twice",
    );

    let (after, decided) = pass_narrowed_repair(&snapshot, bloom, "weave", 44, 45);
    assert!(matches!(decided.outcome, Outcome::CompositionRepaired { tree, .. } if tree == digest(44)));
    assert_eq!(
        verify_targets(&decided),
        vec![workpiece("gamma"), workpiece("delta")],
        "each waiter is resumed once against the repaired tree: {:?}",
        decided.effects,
    );
    assert!(
        !verify_targets(&decided).iter().any(WorkpieceId::is_composition),
        "the composition itself is not re-dispatched",
    );
    assert!(
        recorded_waiters(&after, &bloom).is_empty(),
        "the accepted repair retires the pending set: {:?}",
        recorded_waiters(&after, &bloom),
    );
    let record = after.blooms.get(&bloom).expect("the sealed bloom is still in the snapshot");
    assert_eq!(
        record.progress.get(&workpiece("gamma")).map(|progress| progress.candidate),
        Some(Some(CandidateRef { tree: digest(44), checkout: digest(45) })),
    );
    assert_eq!(
        record.progress.get(&workpiece("delta")).map(|progress| progress.candidate),
        Some(Some(CandidateRef { tree: digest(44), checkout: digest(45) })),
    );
}

// Tripwire: a later attribution of the same parents on a different tree used
// to replace the narrowed-composition row and drop the first waiter. Both
// members still judged a tree they do not own, so both still have to re-enter
// Verify once the repair that overwrote the cursor returns.
#[test]
fn two_different_tree_waiters_both_reverify_after_replacement() {
    let (snapshot, bloom) = members_at_verify(&["alpha", "beta", "gamma", "delta"]);
    let (snapshot, first) = narrow(&snapshot, bloom, "narrow-gamma", "gamma", 40, 41);
    assert!(matches!(first.outcome, Outcome::CompositionNarrowed { attempt: 1, .. }), "{:?}", first.outcome);
    let (snapshot, second) = narrow(&snapshot, bloom, "narrow-delta", "delta", 42, 43);
    assert!(matches!(second.outcome, Outcome::CompositionNarrowed { attempt: 2, .. }), "{:?}", second.outcome);
    assert_eq!(
        recorded_waiters(&snapshot, &bloom),
        vec![workpiece("gamma"), workpiece("delta")],
        "replacement keeps the first waiter and records the second",
    );

    let (after, decided) = pass_narrowed_repair(&snapshot, bloom, "weave", 44, 45);
    assert!(matches!(decided.outcome, Outcome::CompositionRepaired { tree, .. } if tree == digest(44)));
    assert_eq!(
        verify_targets(&decided),
        vec![workpiece("gamma"), workpiece("delta")],
        "both waiters resume after the replacement repair: {:?}",
        decided.effects,
    );
    assert!(recorded_waiters(&after, &bloom).is_empty(), "the accepted replacement repair retires the pending set");
}

// Journal replay is apply-only: waiter capture has to survive without
// re-deciding, including the same-tree dedup that used to drop the second
// member.
#[test]
fn a_replayed_journal_reproduces_two_composition_waiters() {
    let (mut live, bloom) = members_at_verify(&["alpha", "beta", "gamma", "delta"]);
    let events = [
        event(
            "narrow-gamma",
            Fact::CompositionNarrowed {
                bloom,
                verified: workpiece("gamma"),
                tree: digest(40),
                head: digest(41),
                evidence: Evidence { subject: digest(40), kind: EvidenceKind::VerificationResult, detail: digest(50) },
                attribution: collision_parents(),
            },
        ),
        event(
            "narrow-delta",
            Fact::CompositionNarrowed {
                bloom,
                verified: workpiece("delta"),
                tree: digest(40),
                head: digest(41),
                evidence: Evidence { subject: digest(40), kind: EvidenceKind::VerificationResult, detail: digest(50) },
                attribution: collision_parents(),
            },
        ),
        event(
            "weave",
            Fact::AttemptCompleted {
                bloom,
                workpiece: collision_subject(),
                stage: StageId::Refine,
                passed: true,
                evidence: Evidence { subject: digest(44), kind: EvidenceKind::VerificationResult, detail: digest(57) },
                candidate: Some(CandidateRef { tree: digest(44), checkout: digest(45) }),
            },
        ),
    ];

    let mut recorded = Vec::new();
    for next in &events {
        let (snapshot, decisions) = step(&live, next);
        recorded.push((next.clone(), decisions));
        live = snapshot;
    }

    let mut replayed = members_at_verify(&["alpha", "beta", "gamma", "delta"]).0;
    for (event, decisions) in &recorded {
        replayed = replayed.apply(event, decisions, &compiled_resolved());
    }

    assert_eq!(live, replayed, "apply-only replay rebuilds the live snapshot, waiters included");
    assert!(recorded_waiters(&live, &bloom).is_empty(), "replay retires the pending set after the accepted repair");
    assert!(
        matches!(recorded[1].1.outcome, Outcome::CompositionRepairAlreadyInFlight { .. }),
        "the replayed journal includes the dedup outcome: {:?}",
        recorded[1].1.outcome,
    );
    assert_eq!(
        verify_targets(&recorded[2].1),
        vec![workpiece("gamma"), workpiece("delta")],
        "replayed repair decisions still resume both waiters",
    );
}

// Tripwire: a waiter that has left Verify, been withdrawn, or already
// integrated must not be sent back onto the repaired tree. Resume skips
// work that is no longer waiting; the accepted completion then retires
// the whole pending set.
#[test]
fn stale_composition_waiters_are_not_resumed() {
    let (snapshot, bloom) = members_at_verify(&["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]);
    let (snapshot, _) = narrow(&snapshot, bloom, "narrow-gamma", "gamma", 40, 41);
    let (snapshot, _) = narrow(&snapshot, bloom, "narrow-delta", "delta", 40, 41);
    let (snapshot, _) = narrow(&snapshot, bloom, "narrow-epsilon", "epsilon", 40, 41);
    let (snapshot, _) = narrow(&snapshot, bloom, "narrow-zeta", "zeta", 40, 41);
    assert_eq!(
        recorded_waiters(&snapshot, &bloom),
        vec![workpiece("gamma"), workpiece("delta"), workpiece("epsilon"), workpiece("zeta")],
    );

    let (snapshot, withdrawn) = step(
        &snapshot,
        &event(
            "withdraw-gamma",
            Fact::Withdraw {
                bloom,
                withdrawals: vec![Withdrawal {
                    workpiece: workpiece("gamma"),
                    cause: WithdrawalCause::Operator,
                    reason: "this member is no longer in the bloom".into(),
                    operator: "iamacoffeepot".into(),
                }],
                cascade: false,
            },
        ),
    );
    assert!(
        matches!(&withdrawn.outcome, Outcome::MembersWithdrawn { .. }),
        "gamma left the bloom: {:?}",
        withdrawn.outcome,
    );

    let (snapshot, integrated) =
        step(&snapshot, &event("integrate-delta", Fact::Integrate { bloom, claim: claim("delta", 13, 23) }));
    assert!(
        matches!(&integrated.outcome, Outcome::Integrated { .. }),
        "delta already has a claim: {:?}",
        integrated.outcome,
    );

    let (snapshot, failed) = step(
        &snapshot,
        &event(
            "epsilon-failed",
            Fact::VerifyFailed {
                bloom,
                workpiece: workpiece("epsilon"),
                evidence: Evidence { subject: digest(24), kind: EvidenceKind::VerificationResult, detail: digest(61) },
                failed_verifiers: VerifyFailureSet::one(VerifyFailure::Test),
            },
        ),
    );
    assert!(
        matches!(&failed.outcome, Outcome::RefineReentered { .. }),
        "epsilon left Verify for Refine: {:?}",
        failed.outcome,
    );

    let (after, decided) = pass_narrowed_repair(&snapshot, bloom, "weave", 44, 45);
    assert!(matches!(decided.outcome, Outcome::CompositionRepaired { tree, .. } if tree == digest(44)));
    assert_eq!(
        verify_targets(&decided),
        vec![workpiece("zeta")],
        "only the still-waiting Verify member resumes: {:?}",
        decided.effects,
    );
    assert!(recorded_waiters(&after, &bloom).is_empty(), "skipped waiters are retired with the settled pending set");
    let record = after.blooms.get(&bloom).expect("the sealed bloom is still in the snapshot");
    assert!(record.withdrawn.contains_key(&workpiece("gamma")), "the withdrawn waiter stays withdrawn");
    assert!(record.claims.contains_key(&workpiece("delta")), "the resolved waiter keeps its claim");
    assert_eq!(
        record.progress.get(&workpiece("epsilon")).map(|progress| progress.stage),
        Some(StageId::Refine),
        "the non-Verify waiter is not pulled back to Verify",
    );
    assert_eq!(
        record.progress.get(&workpiece("zeta")).map(|progress| progress.candidate),
        Some(Some(CandidateRef { tree: digest(44), checkout: digest(45) })),
    );
}

// Tripwire: waiters that a successful repair already resumed must not stay
// pending. A later attribution over the same parents would otherwise reset
// those members onto a tree they are no longer waiting for, clobbering the
// candidate the first repair put them on.
#[test]
fn a_later_repair_over_the_same_parents_resumes_only_the_new_waiter() {
    let (snapshot, bloom) = members_at_verify(&["alpha", "beta", "gamma", "delta"]);
    let (snapshot, _) = narrow(&snapshot, bloom, "narrow-gamma", "gamma", 40, 41);
    let (snapshot, first) = pass_narrowed_repair(&snapshot, bloom, "weave-gamma", 44, 45);
    assert!(matches!(first.outcome, Outcome::CompositionRepaired { tree, .. } if tree == digest(44)));
    assert_eq!(verify_targets(&first), vec![workpiece("gamma")]);
    assert!(recorded_waiters(&snapshot, &bloom).is_empty(), "gamma is no longer pending after its repair");
    assert_eq!(
        snapshot
            .blooms
            .get(&bloom)
            .expect("the sealed bloom is still in the snapshot")
            .progress
            .get(&workpiece("gamma"))
            .and_then(|progress| progress.candidate),
        Some(CandidateRef { tree: digest(44), checkout: digest(45) }),
        "gamma stays on the candidate the first repair gave it",
    );

    let (snapshot, second_attr) = narrow(&snapshot, bloom, "narrow-delta", "delta", 50, 51);
    assert!(
        matches!(second_attr.outcome, Outcome::CompositionNarrowed { attempt: 2, .. }),
        "a later tree over the same parents buys a new repair lap: {:?}",
        second_attr.outcome,
    );
    assert_eq!(
        recorded_waiters(&snapshot, &bloom),
        vec![workpiece("delta")],
        "the new pending set is only the member that attributed this repair",
    );

    let (after, decided) = pass_narrowed_repair(&snapshot, bloom, "weave-delta", 54, 55);
    assert!(matches!(decided.outcome, Outcome::CompositionRepaired { tree, .. } if tree == digest(54)));
    assert_eq!(
        verify_targets(&decided),
        vec![workpiece("delta")],
        "the later repair must not reset gamma: {:?}",
        decided.effects,
    );
    let record = after.blooms.get(&bloom).expect("the sealed bloom is still in the snapshot");
    assert_eq!(
        record.progress.get(&workpiece("gamma")).and_then(|progress| progress.candidate),
        Some(CandidateRef { tree: digest(44), checkout: digest(45) }),
        "gamma keeps the first repair's candidate",
    );
    assert_eq!(
        record.progress.get(&workpiece("delta")).and_then(|progress| progress.candidate),
        Some(CandidateRef { tree: digest(54), checkout: digest(55) }),
    );
    assert!(recorded_waiters(&after, &bloom).is_empty());
}

// Tripwire: a completion the reducer refused, or a duplicate of one, must not
// retire waiters a real repair has not yet resumed. Apply still sees the
// event; only CompositionRepaired may clear the pending set.
#[test]
fn a_rejected_or_duplicate_completion_does_not_retire_pending_waiters() {
    let (snapshot, bloom) = members_at_verify(&["alpha", "beta", "gamma", "delta"]);
    let (snapshot, _) = narrow(&snapshot, bloom, "narrow-gamma", "gamma", 40, 41);
    let (snapshot, _) = narrow(&snapshot, bloom, "narrow-delta", "delta", 40, 41);
    assert_eq!(recorded_waiters(&snapshot, &bloom), vec![workpiece("gamma"), workpiece("delta")]);

    let rejected = event(
        "weave-wrong-stage",
        Fact::AttemptCompleted {
            bloom,
            workpiece: collision_subject(),
            stage: StageId::Construct,
            passed: true,
            evidence: Evidence { subject: digest(44), kind: EvidenceKind::VerificationResult, detail: digest(57) },
            candidate: Some(CandidateRef { tree: digest(44), checkout: digest(45) }),
        },
    );
    let (snapshot, refused) = step(&snapshot, &rejected);
    assert!(
        matches!(
            refused.outcome,
            Outcome::AttemptCompletedRejected(AttemptCompletedError::StageMismatch { expected: StageId::Refine, .. })
        ),
        "the composition is still at Refine: {:?}",
        refused.outcome,
    );
    assert_eq!(
        recorded_waiters(&snapshot, &bloom),
        vec![workpiece("gamma"), workpiece("delta")],
        "a refused completion leaves the pending set standing",
    );

    let (snapshot, duplicate) = step(&snapshot, &rejected);
    assert!(matches!(duplicate.outcome, Outcome::Duplicate), "{:?}", duplicate.outcome);
    assert_eq!(
        recorded_waiters(&snapshot, &bloom),
        vec![workpiece("gamma"), workpiece("delta")],
        "a duplicate of the refusal still leaves the pending set standing",
    );

    let (after, decided) = pass_narrowed_repair(&snapshot, bloom, "weave", 44, 45);
    assert!(matches!(decided.outcome, Outcome::CompositionRepaired { tree, .. } if tree == digest(44)));
    assert_eq!(verify_targets(&decided), vec![workpiece("gamma"), workpiece("delta")]);
    assert!(recorded_waiters(&after, &bloom).is_empty());
}
