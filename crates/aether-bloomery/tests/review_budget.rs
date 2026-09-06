//! Candidate-review parking is the hard two-pass ceiling (ADR-0153), not the
//! sealed `AggregateReview` retry budget. That budget is the executor-fault
//! ledger (ADR-0176). Catalog validation admits values through 16, so a
//! budget of 3 used to let the second blocking verdict re-weave and the
//! repaired weave dispatch review roll three.

mod common;

use aether_bloomery::{
    BloomId, CandidateRef, Decision, Event, Evidence, EvidenceKind, Fact, Outcome, Snapshot, SpendWindow, StageCatalog,
    StageId, WorkpieceId, reduce,
};
use common::{claim, digest, draft_with_catalog, event, membership, step};

/// The compiled line with `AggregateReview.retry_budget` raised above two —
/// valid to seal, and the value that used to buy a third candidate judgment.
fn catalog_with_review_budget(budget: u32) -> StageCatalog {
    let mut catalog = StageCatalog::line();
    catalog
        .bindings
        .iter_mut()
        .find(|binding| binding.stage == StageId::AggregateReview)
        .expect("compiled line binds AggregateReview")
        .retry_budget = budget;
    catalog
}

fn sealed_with_review_budget(budget: u32) -> (Snapshot, BloomId) {
    let catalog = catalog_with_review_budget(budget);
    let (draft, configs) = draft_with_catalog(1, vec![membership("alpha", 10), membership("beta", 11)], &catalog);
    let spec = draft.seal();
    let bloom = spec.id();
    let base = Snapshot::new(digest(1)).with_green_base(digest(1));
    let seal = event("seal", Fact::Seal(spec));
    let decisions = reduce(&base, &seal, &configs, &SpendWindow::default());
    assert!(
        matches!(decisions.outcome, Outcome::Sealed(_)),
        "a catalog with AggregateReview budget {budget} must seal: {:?}",
        decisions.outcome,
    );
    (base.apply(&seal, &decisions, &configs), bloom)
}

/// Members integrated, fold held, mechanical gate already passed — the
/// position the critic's first verdict arrives at.
fn held_fold_under_review(budget: u32) -> (Snapshot, BloomId) {
    let (snapshot, bloom) = sealed_with_review_budget(budget);
    let (snapshot, _) = step(&snapshot, &event("i-a", Fact::Integrate { bloom, claim: claim("alpha", 10, 100) }));
    let (snapshot, _) = step(&snapshot, &event("i-b", Fact::Integrate { bloom, claim: claim("beta", 11, 101) }));
    let (snapshot, _) =
        step(&snapshot, &event("r1", Fact::Resolve { bloom, tree: digest(40), head: digest(41), lineage: vec![] }));
    let (snapshot, _) = step(&snapshot, &verify_passed(bloom, "v1", 40));
    (snapshot, bloom)
}

fn verify_passed(bloom: BloomId, key: &str, tree: u8) -> Event {
    event(
        key,
        Fact::AggregateVerifyCompleted {
            bloom,
            passed: true,
            evidence: Evidence { subject: digest(tree), kind: EvidenceKind::VerificationResult, detail: digest(51) },
        },
    )
}

fn review_failed(bloom: BloomId, key: &str, tree: u8, detail: u8) -> Event {
    event(
        key,
        Fact::AggregateReviewCompleted {
            bloom,
            passed: false,
            evidence: Evidence { subject: digest(tree), kind: EvidenceKind::ReviewFinding, detail: digest(detail) },
            implicated: vec![],
        },
    )
}

fn weave_repaired(bloom: BloomId, key: &str, from: u8, tree: u8, head: u8) -> Event {
    event(
        key,
        Fact::AttemptCompleted {
            bloom,
            workpiece: WorkpieceId::composition(),
            stage: StageId::Refine,
            passed: true,
            evidence: Evidence { subject: digest(from), kind: EvidenceKind::VerificationResult, detail: digest(57) },
            candidate: Some(CandidateRef { tree: digest(tree), checkout: digest(head) }),
        },
    )
}

fn executor_fault(bloom: BloomId, key: &str, subject: u8, detail: u8) -> Event {
    event(
        key,
        Fact::AggregateReviewExecutorFault {
            bloom,
            evidence: Evidence { subject: digest(subject), kind: EvidenceKind::ExecutorFault, detail: digest(detail) },
        },
    )
}

// This fails if someone keys candidate-review parking on the sealed
// AggregateReview retry_budget again: a valid budget of 3 would re-weave
// after the second blocking verdict and the repaired weave would dispatch
// review roll three without an owner answer.
#[test]
fn a_catalog_budget_of_three_still_parks_after_two_candidate_judgments() {
    let (snapshot, bloom) = held_fold_under_review(3);
    assert_eq!(
        snapshot.blooms.get(&bloom).expect("sealed").stage_catalog.retry_budget_of(StageId::AggregateReview),
        Some(3),
        "the bloom is running the authored budget that used to raise the ceiling",
    );

    let (after1, d1) = step(&snapshot, &review_failed(bloom, "fail-1", 40, 50));
    assert!(
        matches!(d1.outcome, Outcome::CompositionRewoven { refused_at: StageId::AggregateReview, attempt: 1, .. }),
        "the first blocking verdict still repairs the weave: {:?}",
        d1.outcome,
    );

    let (after2, d2) = step(&after1, &weave_repaired(bloom, "weave-1", 40, 44, 45));
    assert!(
        d2.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { roll: 2, .. })),
        "the repaired weave sends the delta-confirm, not a third roll: {:?}",
        d2.effects,
    );
    let (after3, _) = step(&after2, &verify_passed(bloom, "v2", 44));

    let (parked, d4) = step(&after3, &review_failed(bloom, "fail-2", 44, 51));
    assert!(
        matches!(d4.outcome, Outcome::AggregateReviewParked { rolls: 2, question, .. } if question == digest(51)),
        "the second blocking verdict parks at the two-pass ceiling even though the catalog budget is 3: {:?}",
        d4.outcome,
    );
    assert!(
        !d4.effects.iter().any(|effect| {
            matches!(effect, Decision::DispatchAttempt { .. } | Decision::DispatchAggregateReview { .. })
        }),
        "parking must not re-weave or dispatch review roll three: {:?}",
        d4.effects,
    );
    let record = parked.blooms.get(&bloom).expect("sealed");
    assert_eq!(record.aggregate_rolls, 2);
    assert_eq!(record.review_park, Some(digest(51)));
}

// This fails if the candidate two-pass ceiling is copied onto the executor-fault
// ledger: a sealed budget of 3 would wedge on the second environment fault
// instead of redispatching the same held fold once more.
#[test]
fn a_catalog_budget_of_three_still_bounds_executor_faults() {
    let (snapshot, bloom) = held_fold_under_review(3);

    let (after1, d1) = step(&snapshot, &executor_fault(bloom, "fault-1", 40, 60));
    assert!(
        matches!(
            d1.outcome,
            Outcome::AggregateReviewExecutorFaulted { fault, budget: 3, .. }
                if fault.rolls == 1 && fault.subject == digest(40)
        ),
        "the first fault spends the catalog ledger, not a candidate roll: {:?}",
        d1.outcome,
    );
    assert!(d1.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { roll: 1, .. })));

    let (after2, d2) = step(&after1, &executor_fault(bloom, "fault-2", 40, 61));
    assert!(
        matches!(
            d2.outcome,
            Outcome::AggregateReviewExecutorFaulted { fault, budget: 3, .. } if fault.rolls == 2
        ),
        "budget 3 still redispatches after the second fault: {:?}",
        d2.outcome,
    );
    assert!(
        d2.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "the second fault must not inherit the candidate two-pass park: {:?}",
        d2.effects,
    );
    assert_eq!(after2.blooms.get(&bloom).expect("sealed").aggregate_rolls, 0);

    let (_, d3) = step(&after2, &executor_fault(bloom, "fault-3", 40, 62));
    assert!(
        matches!(
            d3.outcome,
            Outcome::AggregateReviewExecutorWedged { fault, budget: 3, .. } if fault.rolls == 3
        ),
        "the catalog ceiling still terminates the fault series: {:?}",
        d3.outcome,
    );
    assert!(!d3.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })));
}
