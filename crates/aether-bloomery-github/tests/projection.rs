//! Projection reconcile against the fake GitHub (#3459 step 5 coverage): the
//! carbon-copy count, idempotency, in-place update, and the delete → reappear
//! rebuild property.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, MemberView, PendingDecisionView,
    ProjectionBackend, ResolutionClaim, StageId, ViewDocument, WorkpieceId,
};
use aether_bloomery_github::{GithubProjection, testing::FakeGithub};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn approval(subject: Digest) -> Evidence {
    Evidence { subject, kind: EvidenceKind::Approval, detail: digest(200) }
}

/// A two-member bloom; the second member is integrated (carries a resolution)
/// when `resolve_second` is set.
fn view(resolve_second: bool) -> ViewDocument {
    let member_a = MemberView {
        workpiece: WorkpieceId("reactor-core".into()),
        scope_revision: digest(10),
        approval: approval(digest(10)),
        resolution: None,
        pending_decision: None,
    };
    let mut member_b = MemberView {
        workpiece: WorkpieceId("coolant-loop".into()),
        scope_revision: digest(20),
        approval: approval(digest(20)),
        resolution: None,
        pending_decision: None,
    };
    if resolve_second {
        member_b.resolution = Some(ResolutionClaim {
            workpiece: WorkpieceId("coolant-loop".into()),
            scope_revision: digest(20),
            candidate: digest(21),
            evidence: Evidence { subject: digest(21), kind: EvidenceKind::ResolutionClaim, detail: digest(210) },
        });
    }
    let bloom = BloomView {
        id: BloomId(digest(1)),
        status: BloomStatus::Sealed,
        superseded_by: None,
        members: vec![member_a, member_b],
    };
    ViewDocument { mainline: digest(0), blooms: vec![bloom] }
}

#[test]
fn first_reconcile_creates_the_carbon_copy() {
    let projection = GithubProjection::new(FakeGithub::new());
    projection.reconcile_view(&view(false)).expect("reconcile");

    // 1 umbrella issue + 2 workpiece issues.
    assert_eq!(projection.client().issue_count(), 3);
    // 1 approval comment per member.
    assert_eq!(projection.client().comment_count(), 2);
}

#[test]
fn reconciling_the_same_document_twice_is_idempotent() {
    let projection = GithubProjection::new(FakeGithub::new());
    let document = view(false);

    projection.reconcile_view(&document).expect("first reconcile");
    let after_first = (projection.client().issue_count(), projection.client().comment_count());
    let numbers_first = projection.client().issue_numbers();

    projection.reconcile_view(&document).expect("second reconcile");
    let after_second = (projection.client().issue_count(), projection.client().comment_count());

    // No duplicate objects — every find matched its marker, so the second pass
    // was all no-ops.
    assert_eq!(after_first, after_second);
    assert_eq!(numbers_first, projection.client().issue_numbers());
}

#[test]
fn a_changed_view_updates_in_place() {
    let projection = GithubProjection::new(FakeGithub::new());

    projection.reconcile_view(&view(false)).expect("initial reconcile");
    let numbers_before = projection.client().issue_numbers();

    // Integrating the second member changes its member-issue digest and adds a
    // resolution comment.
    projection.reconcile_view(&view(true)).expect("changed reconcile");

    // Same issue numbers — the change was an in-place update, not a recreate.
    assert_eq!(numbers_before, projection.client().issue_numbers());
    // The resolution comment was added.
    assert_eq!(projection.client().comment_count(), 3);
    // The coolant-loop member issue now reads as integrated.
    let coolant_body = projection
        .client()
        .issue_numbers()
        .into_iter()
        .filter_map(|n| projection.client().issue_body(n))
        .find(|body| body.contains("Workpiece `coolant-loop`"))
        .expect("the coolant-loop member issue exists");
    assert!(coolant_body.contains("- Resolution: integrated"), "the member issue reflects its resolution");
}

/// The two-member bloom with the first member held on a parked question.
fn held_view() -> ViewDocument {
    let mut document = view(false);
    document.blooms[0].members[0].pending_decision = Some(PendingDecisionView {
        question: digest(90),
        stage: StageId::Construct,
        prompt: "tie between A and B".into(),
        options: vec!["A".into(), "B".into()],
        blocked: "construct is held".into(),
    });
    document
}

#[test]
fn a_held_member_projects_an_idempotent_question_comment() {
    // ADR-0151: a parked question projects one comment on the held member's
    // workpiece issue (visible where a person looks), carrying the question digest
    // in stable metadata. Re-reconciling the same hold is a no-op — the marker's
    // content digest matches, so no second comment is written.
    let projection = GithubProjection::new(FakeGithub::new());

    // The unheld baseline: one approval comment per member.
    projection.reconcile_view(&view(false)).expect("baseline reconcile");
    assert_eq!(projection.client().comment_count(), 2);

    // Reconciling the held view adds exactly one question comment.
    projection.reconcile_view(&held_view()).expect("held reconcile");
    assert_eq!(projection.client().comment_count(), 3, "the held member's question projects one comment");

    // Re-reconciling the identical held view writes nothing new (idempotent).
    projection.reconcile_view(&held_view()).expect("idempotent reconcile");
    assert_eq!(projection.client().comment_count(), 3, "re-reconciling the same hold is a no-op");
}

#[test]
fn a_deleted_projection_reappears_on_the_next_reconcile() {
    let projection = GithubProjection::new(FakeGithub::new());
    let document = view(false);

    projection.reconcile_view(&document).expect("initial reconcile");
    let full = projection.client().issue_count();

    // An operator deletes a projected issue.
    let victim = projection.client().issue_numbers()[1];
    projection.client().delete_issue(victim);
    assert_eq!(projection.client().issue_count(), full - 1);

    // The reconcile finds no marker for the deleted object and recreates it —
    // the rebuild-from-journal property.
    projection.reconcile_view(&document).expect("rebuild reconcile");
    assert_eq!(projection.client().issue_count(), full);
    // The recreated issue is a fresh number (the deleted one is gone).
    assert!(!projection.client().issue_numbers().contains(&victim));
}
