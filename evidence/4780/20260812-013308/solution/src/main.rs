use aether_bloomery::{
    BloomDraft, BloomId, Budget, CandidateRef, ConfigRegistry, Decisions, Digest, Event, Evidence,
    EvidenceKind, Fact, Forecast, IdempotencyKey, MemberView, Membership, Outcome, ResolvedConfigs,
    Snapshot, StageCatalog, StageId, VerifyFailure, VerifyFailureSet, ViewDocument, WorkpieceId,
    reduce, view_of,
};

fn digest(byte: u8) -> Digest {
    Digest::from_bytes([byte; 32])
}

fn verification_evidence(subject: Digest, detail: u8) -> Evidence {
    Evidence {
        subject,
        kind: EvidenceKind::VerificationResult,
        detail: digest(detail),
    }
}

fn approval_evidence(subject: Digest, detail: u8) -> Evidence {
    Evidence {
        subject,
        kind: EvidenceKind::Approval,
        detail: digest(detail),
    }
}

fn approved_membership(workpiece: WorkpieceId, scope_revision: Digest, detail: u8) -> Membership {
    let mut membership = Membership {
        workpiece,
        scope_revision,
        configs: ConfigRegistry::default(),
        approval: approval_evidence(Digest::default(), detail),
    };
    membership.approval.subject = membership.subject();

    membership
}

fn apply_fact(
    snapshot: &mut Snapshot,
    configs: &ResolvedConfigs,
    sequence: &mut u32,
    fact: Fact,
) -> Decisions {
    *sequence += 1;
    let event = Event {
        idempotency_key: IdempotencyKey(format!("consumer-{sequence}")),
        fact,
    };
    let decisions = reduce(snapshot, &event, configs);

    println!("event {sequence}: {:?}", decisions.outcome);
    *snapshot = snapshot.apply(&event, &decisions, configs);

    decisions
}

fn stage_of(snapshot: &Snapshot, bloom: &BloomId, workpiece: &WorkpieceId) -> StageId {
    snapshot.blooms[bloom].progress[workpiece].stage
}

fn complete_construct(
    snapshot: &mut Snapshot,
    configs: &ResolvedConfigs,
    sequence: &mut u32,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    candidate: CandidateRef,
    detail: u8,
) {
    let decisions = apply_fact(
        snapshot,
        configs,
        sequence,
        Fact::AttemptCompleted {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            stage: StageId::Construct,
            passed: true,
            evidence: verification_evidence(candidate.tree, detail),
            candidate: Some(candidate),
        },
    );

    assert!(matches!(
        decisions.outcome,
        Outcome::AttemptAdvanced {
            from: StageId::Construct,
            to: StageId::Verify,
            ..
        }
    ));
    assert_eq!(stage_of(snapshot, bloom, workpiece), StageId::Verify);
}

fn complete_refine(
    snapshot: &mut Snapshot,
    configs: &ResolvedConfigs,
    sequence: &mut u32,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    candidate: CandidateRef,
    detail: u8,
) {
    let decisions = apply_fact(
        snapshot,
        configs,
        sequence,
        Fact::AttemptCompleted {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            stage: StageId::Refine,
            passed: true,
            evidence: verification_evidence(candidate.tree, detail),
            candidate: Some(candidate),
        },
    );

    assert!(matches!(
        decisions.outcome,
        Outcome::AttemptAdvanced {
            from: StageId::Refine,
            to: StageId::Verify,
            ..
        }
    ));
    assert_eq!(stage_of(snapshot, bloom, workpiece), StageId::Verify);
}

fn fail_verify(
    snapshot: &mut Snapshot,
    configs: &ResolvedConfigs,
    sequence: &mut u32,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    candidate: CandidateRef,
    failure: (VerifyFailure, u8),
) -> Decisions {
    apply_fact(
        snapshot,
        configs,
        sequence,
        Fact::VerifyFailed {
            bloom: *bloom,
            workpiece: workpiece.clone(),
            evidence: verification_evidence(candidate.tree, failure.1),
            failed_verifiers: VerifyFailureSet::one(failure.0),
        },
    )
}

fn assert_novel_failure(decisions: &Decisions) {
    assert!(matches!(
        &decisions.outcome,
        Outcome::RefineReentered { rolls: 0, .. }
    ));
}

fn repeat_clippy_until_wedged(
    snapshot: &mut Snapshot,
    configs: &ResolvedConfigs,
    sequence: &mut u32,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    candidate: CandidateRef,
    next_detail: &mut u8,
) -> u32 {
    for repeat in 1..=StageCatalog::MAX_RETRY_BUDGET + 1 {
        let decisions = fail_verify(
            snapshot,
            configs,
            sequence,
            bloom,
            workpiece,
            candidate,
            (VerifyFailure::Clippy, *next_detail),
        );
        *next_detail += 1;

        match decisions.outcome {
            Outcome::RefineReentered { rolls, .. } => {
                assert!(rolls > 0);
                complete_refine(
                    snapshot,
                    configs,
                    sequence,
                    bloom,
                    workpiece,
                    candidate,
                    *next_detail,
                );
                *next_detail += 1;
            }
            Outcome::AttemptWedged {
                stage: StageId::Verify,
                repeated_verifiers,
                ..
            } => {
                assert!(repeated_verifiers.contains(VerifyFailure::Clippy));
                return repeat;
            }
            other => panic!("unexpected repeated clippy outcome: {other:?}"),
        }
    }

    panic!("repeated clippy never exhausted the verify repair budget");
}

fn outward_member<'a>(
    view: &'a ViewDocument,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
) -> &'a MemberView {
    view.blooms
        .iter()
        .find(|candidate| &candidate.id == bloom)
        .expect("bloom appears in outward view")
        .members
        .iter()
        .find(|candidate| &candidate.workpiece == workpiece)
        .expect("member appears in outward view")
}

fn main() {
    let configs = ResolvedConfigs::default();
    let verify_budget = StageCatalog::line()
        .retry_budget_of(StageId::Verify)
        .expect("the default line binds Verify");
    let target = WorkpieceId("target".to_owned());
    let sibling = WorkpieceId("sibling".to_owned());
    let target_scope = digest(1);
    let sibling_scope = digest(2);
    let draft = BloomDraft {
        proposals: vec![
            approved_membership(target.clone(), target_scope, 3),
            approved_membership(sibling.clone(), sibling_scope, 4),
        ],
        base: Snapshot::GENESIS_MAINLINE,
        configs: ConfigRegistry::default(),
        budget: Budget {
            token_ceiling: 100_000,
            wall_clock_secs: 3_600,
            retry_cap: verify_budget,
        },
        forecast: Forecast {
            predicted_cost: 0,
            predicted_secs: 0,
            predicted_retries: 0,
        },
    };
    let spec = draft.seal();
    let bloom = spec.id();
    let mut snapshot = Snapshot::default();
    let mut sequence = 0;
    let seal = apply_fact(&mut snapshot, &configs, &mut sequence, Fact::Seal(spec));

    assert!(matches!(seal.outcome, Outcome::Sealed(_)));

    let target_candidate = CandidateRef {
        tree: digest(10),
        checkout: digest(11),
    };
    let sibling_candidate = CandidateRef {
        tree: digest(20),
        checkout: digest(21),
    };
    let mut next_detail = 30;

    complete_construct(
        &mut snapshot,
        &configs,
        &mut sequence,
        &bloom,
        &target,
        target_candidate,
        next_detail,
    );
    next_detail += 1;
    complete_construct(
        &mut snapshot,
        &configs,
        &mut sequence,
        &bloom,
        &sibling,
        sibling_candidate,
        next_detail,
    );
    next_detail += 1;

    for failure in [
        VerifyFailure::Fmt,
        VerifyFailure::Clippy,
        VerifyFailure::Docs,
    ] {
        let decisions = fail_verify(
            &mut snapshot,
            &configs,
            &mut sequence,
            &bloom,
            &target,
            target_candidate,
            (failure, next_detail),
        );
        next_detail += 1;
        assert_novel_failure(&decisions);
        complete_refine(
            &mut snapshot,
            &configs,
            &mut sequence,
            &bloom,
            &target,
            target_candidate,
            next_detail,
        );
        next_detail += 1;
    }

    let sibling_clippy = fail_verify(
        &mut snapshot,
        &configs,
        &mut sequence,
        &bloom,
        &sibling,
        sibling_candidate,
        (VerifyFailure::Clippy, next_detail),
    );
    next_detail += 1;
    assert_novel_failure(&sibling_clippy);

    let repeats_before_grant = repeat_clippy_until_wedged(
        &mut snapshot,
        &configs,
        &mut sequence,
        &bloom,
        &target,
        target_candidate,
        &mut next_detail,
    );
    let before_grant = view_of(&snapshot, |_| None);
    let target_before_grant = outward_member(&before_grant, &bloom, &target);
    let sibling_before_grant = outward_member(&before_grant, &bloom, &sibling);

    assert_eq!(
        target_before_grant
            .wedge
            .expect("target is outwardly wedged")
            .stage,
        StageId::Verify
    );
    assert!(
        target_before_grant
            .wedge
            .expect("target is outwardly wedged")
            .repeated_verifiers
            .contains(VerifyFailure::Clippy)
    );
    assert!(sibling_before_grant.wedge.is_none());

    let grant = apply_fact(
        &mut snapshot,
        &configs,
        &mut sequence,
        Fact::GrantAttempts {
            bloom,
            workpiece: target.clone(),
            stage: StageId::Verify,
            attempts: verify_budget,
        },
    );

    assert!(matches!(
        grant.outcome,
        Outcome::AttemptsGranted {
            resumes_at: StageId::Refine,
            attempts,
            ..
        } if attempts == verify_budget
    ));
    assert_eq!(stage_of(&snapshot, &bloom, &target), StageId::Refine);

    let after_grant = view_of(&snapshot, |_| None);
    assert!(
        outward_member(&after_grant, &bloom, &target)
            .wedge
            .is_none()
    );

    complete_refine(
        &mut snapshot,
        &configs,
        &mut sequence,
        &bloom,
        &target,
        target_candidate,
        next_detail,
    );
    next_detail += 1;
    let repeats_after_grant = repeat_clippy_until_wedged(
        &mut snapshot,
        &configs,
        &mut sequence,
        &bloom,
        &target,
        target_candidate,
        &mut next_detail,
    );
    let final_view = view_of(&snapshot, |_| None);

    assert!(outward_member(&final_view, &bloom, &target).wedge.is_some());
    assert!(
        outward_member(&final_view, &bloom, &sibling)
            .wedge
            .is_none()
    );

    println!(
        "verify_budget={verify_budget}, repeats_before_grant={repeats_before_grant}, repeats_after_grant={repeats_after_grant}"
    );
    println!(
        "before_grant={}\nafter_grant={}\nfinal_view={}",
        serde_json::to_string_pretty(&before_grant).expect("serialize pre-grant view"),
        serde_json::to_string_pretty(&after_grant).expect("serialize post-grant view"),
        serde_json::to_string_pretty(&final_view).expect("serialize final view")
    );
}
