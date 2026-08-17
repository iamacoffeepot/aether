//! Projection reconcile against the fake GitHub (#3459 step 5 coverage, as
//! narrowed by #4663): the objects the projection must leave alone, the one
//! folded comment per member it writes, idempotency, in-place update, the
//! receipt's fan-out onto member issues and the landing pull request, and the
//! delete → reappear rebuild property.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, LandingReceipt, MemberView, PendingDecisionView,
    ProjectedReceipt, ProjectionBackend, ResolutionClaim, StageId, ViewDocument, WorkpieceId,
};
use aether_bloomery_github::{GithubProjection, landing_branch, testing::FakeGithub};

/// The two issue numbers the view's members address — objects the repository
/// already holds, which the projection comments on and never opens.
const MEMBER_A: u64 = 4628;
const MEMBER_B: u64 = 4629;

/// A body a person wrote. The projection has no verb that can touch it.
const AUTHORED_BODY: &str = "A person wrote this issue body.";

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn approval(subject: Digest) -> Evidence {
    Evidence { subject, kind: EvidenceKind::Approval, detail: digest(200) }
}

/// A fake already holding both member issues.
fn seeded() -> FakeGithub {
    let fake = FakeGithub::new();
    fake.seed_issue(MEMBER_A, AUTHORED_BODY);
    fake.seed_issue(MEMBER_B, AUTHORED_BODY);
    fake
}

fn member(number: u64, revision: u8) -> MemberView {
    MemberView {
        workpiece: WorkpieceId(format!("issue-{number}")),
        scope_revision: digest(revision),
        approval: approval(digest(revision)),
        resolution: None,
        pending_decision: None,
        wedge: None,
        blocked_by: None,
        host_fault: None,
        machinery_rolls: 0,
        machinery_budget: 0,
        wedge_cause: None,
    }
}

fn one_bloom(id: BloomId, members: Vec<MemberView>) -> ViewDocument {
    let bloom = BloomView {
        id,
        status: BloomStatus::Sealed,
        superseded_by: None,
        members,
        landing_blocked: None,
        executor_fault: None,
        review_park: None,
        composition: None,
    };
    ViewDocument { mainline: digest(0), observed: digest(0), spend_quiesce: None, blooms: vec![bloom] }
}

/// A two-member bloom; the second member is integrated (carries a resolution)
/// when `resolve_second` is set.
fn view(resolve_second: bool) -> ViewDocument {
    let mut member_b = member(MEMBER_B, 20);
    if resolve_second {
        member_b.resolution = Some(ResolutionClaim {
            workpiece: member_b.workpiece.clone(),
            scope_revision: digest(20),
            candidate: digest(21),
            evidence: Evidence { subject: digest(21), kind: EvidenceKind::ResolutionClaim, detail: digest(210) },
        });
    }
    one_bloom(BloomId(digest(1)), vec![member(MEMBER_A, 10), member_b])
}

#[test]
fn a_reconcile_comments_on_existing_objects_and_opens_none() {
    let projection = GithubProjection::new(seeded());
    projection.reconcile_view(&view(false)).expect("reconcile");

    assert_eq!(projection.client().issue_count(), 2, "the projection opens no object of its own");
    assert_eq!(projection.client().comments_on(MEMBER_A).len(), 1, "one folded comment per member");
    assert_eq!(projection.client().comments_on(MEMBER_B).len(), 1);
    assert_eq!(
        projection.client().issue_body(MEMBER_A).as_deref(),
        Some(AUTHORED_BODY),
        "the authored body is never written back",
    );
}

#[test]
fn reconciling_the_same_document_twice_is_idempotent() {
    let projection = GithubProjection::new(seeded());
    let document = view(false);

    projection.reconcile_view(&document).expect("first reconcile");
    let after_first = projection.client().comments_on(MEMBER_A);

    projection.reconcile_view(&document).expect("second reconcile");

    // No duplicate comments — every find matched its marker, so the second pass
    // was all no-ops.
    assert_eq!(projection.client().comment_count(), 2);
    assert_eq!(projection.client().comments_on(MEMBER_A), after_first);
}

#[test]
fn a_changed_member_updates_its_comment_in_place() {
    let projection = GithubProjection::new(seeded());

    projection.reconcile_view(&view(false)).expect("initial reconcile");
    let ids_before = projection.client().comment_ids_on(MEMBER_B);

    projection.reconcile_view(&view(true)).expect("changed reconcile");

    assert_eq!(
        projection.client().comment_ids_on(MEMBER_B),
        ids_before,
        "integrating the member edits its comment rather than adding a second",
    );
    assert!(
        projection.client().comments_on(MEMBER_B)[0].contains("- State: integrated"),
        "the comment reflects the member's resolution",
    );
}

#[test]
fn a_blocked_member_names_the_ancestor_holding_it_out_of_the_line() {
    // The plausible bug: a dependent waiting on an unresolved ancestor
    // renders as "in progress", so the issue comment looks idle for a
    // reason the operator cannot name (ADR-0196).
    let projection = GithubProjection::new(seeded());
    let mut document = view(false);
    document.blooms[0].members[1].blocked_by = Some(WorkpieceId(format!("issue-{MEMBER_A}")));

    projection.reconcile_view(&document).expect("blocked reconcile");
    let body = &projection.client().comments_on(MEMBER_B)[0];
    assert!(body.contains(&format!("blocked by `issue-{MEMBER_A}`")), "state names the ancestor: {body}");
    assert!(body.contains("**Blocked** by"), "the hold is stated, not left as silence: {body}");
}

#[test]
fn a_held_member_folds_its_question_into_the_same_comment() {
    // ADR-0151 as narrowed by #4663: a parked question is one more fact about
    // the member — same value, same change — so it rides the member's comment
    // instead of taking one of its own.
    let projection = GithubProjection::new(seeded());

    projection.reconcile_view(&view(false)).expect("baseline reconcile");
    assert_eq!(projection.client().comment_count(), 2);

    let mut held = view(false);
    held.blooms[0].members[0].pending_decision = Some(PendingDecisionView {
        question: digest(90),
        stage: StageId::Construct,
        prompt: "tie between A and B".into(),
        options: vec!["A".into(), "B".into()],
        blocked: "construct is held".into(),
    });

    projection.reconcile_view(&held).expect("held reconcile");
    assert_eq!(projection.client().comment_count(), 2, "the hold writes no comment of its own");
    assert!(
        projection.client().comments_on(MEMBER_A)[0].contains("**Decision needed**"),
        "the held member's own comment carries the question",
    );

    projection.reconcile_view(&held).expect("idempotent reconcile");
    assert_eq!(projection.client().comment_count(), 2, "re-reconciling the same hold is a no-op");
}

#[test]
fn two_blooms_admitting_one_workpiece_keep_a_comment_each() {
    // The bloom half of the member key is load-bearing now that members share
    // the repository's real issues: a successor re-admitting the same workpiece
    // lands on the same object as its predecessor, and a workpiece-only key
    // would have the two overwrite each other.
    let projection = GithubProjection::new(seeded());

    projection.reconcile_view(&one_bloom(BloomId(digest(1)), vec![member(MEMBER_A, 10)])).expect("predecessor");
    projection.reconcile_view(&one_bloom(BloomId(digest(2)), vec![member(MEMBER_A, 11)])).expect("successor");

    let comments = projection.client().comments_on(MEMBER_A);
    assert_eq!(comments.len(), 2, "one comment per bloom on the one shared issue");
    assert_ne!(comments[0], comments[1], "each names its own bloom");
}

#[test]
fn a_receipt_reaches_every_member_issue_and_the_landing_pull_request() {
    const LANDING: u64 = 5000;

    let bloom = BloomId(digest(1));
    let fake = seeded();
    fake.seed_pull_request(LANDING, &landing_branch(&bloom));
    let projection = GithubProjection::new(fake);

    projection
        .project_receipt(&ProjectedReceipt {
            receipt: LandingReceipt { bloom, previous_base: digest(0), new_head: digest(40) },
            members: vec![WorkpieceId(format!("issue-{MEMBER_A}")), WorkpieceId(format!("issue-{MEMBER_B}"))],
        })
        .expect("receipt");

    assert_eq!(projection.client().comments_on(MEMBER_A).len(), 1);
    assert_eq!(projection.client().comments_on(MEMBER_B).len(), 1);
    assert_eq!(projection.client().comments_on(LANDING).len(), 1, "the landing pull request is a target too");
    assert_eq!(projection.client().issue_count(), 2, "a receipt opens nothing");
}

#[test]
fn a_receipt_with_no_landing_pull_request_still_reaches_its_members() {
    // The pull request is a target, not a precondition: a bloom can land through
    // a path that opened none, and requiring one would wedge those lanes.
    let projection = GithubProjection::new(seeded());

    projection
        .project_receipt(&ProjectedReceipt {
            receipt: LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(0), new_head: digest(40) },
            members: vec![WorkpieceId(format!("issue-{MEMBER_A}"))],
        })
        .expect("a bloom that opened no proposal still receipts");

    assert_eq!(projection.client().comments_on(MEMBER_A).len(), 1);
}

#[test]
fn an_unaddressable_id_and_an_absent_object_are_skipped_without_error() {
    // Outbox delivery is at-least-once and holds a topic until its entry
    // succeeds, so a permanent condition must never surface as an error — one
    // unaddressable member would block the mirror indefinitely. Both skips sit
    // ahead of a reachable member, so a skip that aborted its siblings would
    // leave the last comment unwritten.
    let fake = FakeGithub::new();
    fake.seed_issue(MEMBER_B, AUTHORED_BODY);
    let projection = GithubProjection::new(fake);

    let absent = member(MEMBER_A, 10);
    let unaddressable = MemberView { workpiece: WorkpieceId("reactor-core".into()), ..member(MEMBER_A, 30) };
    let document = one_bloom(BloomId(digest(1)), vec![absent, unaddressable, member(MEMBER_B, 20)]);

    projection.reconcile_view(&document).expect("a permanent condition is a skip, not an error");

    assert_eq!(projection.client().issue_count(), 1, "nothing is fabricated to give a projection a home");
    assert_eq!(projection.client().comment_count(), 1, "only the target the repository holds is written");
    assert_eq!(projection.client().comments_on(MEMBER_B).len(), 1, "a skipped target does not abort its siblings");
}

#[test]
fn a_deleted_comment_reappears_on_the_next_reconcile() {
    let projection = GithubProjection::new(seeded());
    let document = view(false);

    projection.reconcile_view(&document).expect("initial reconcile");

    // An operator deletes a projected comment.
    let victim = projection.client().comment_ids_on(MEMBER_A)[0];
    projection.client().delete_comment(victim);
    assert!(projection.client().comments_on(MEMBER_A).is_empty());

    // The reconcile finds no marker for the deleted comment and writes it again
    // — the rebuild-from-journal property.
    projection.reconcile_view(&document).expect("rebuild reconcile");

    let rebuilt = projection.client().comment_ids_on(MEMBER_A);
    assert_eq!(rebuilt.len(), 1, "the deleted projection was rebuilt from the view document alone");
    assert_ne!(rebuilt[0], victim, "the rebuilt comment is a fresh object");
}
