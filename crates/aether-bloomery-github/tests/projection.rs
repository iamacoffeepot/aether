//! Projection onto source issues and the landing PR — the revised mirror.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, LandingReceipt, MemberView, PendingDecisionView,
    ProjectionBackend, StageId, ViewDocument, WorkpieceId,
};
use aether_bloomery_github::{
    GithubApi, GithubProjection, NewPullRequest, PullRequestApi, parse_source_issue_number, testing::FakeGithub,
};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn approval(subject: Digest) -> Evidence {
    Evidence { subject, kind: EvidenceKind::Approval, detail: digest(200) }
}

fn issuable_view() -> ViewDocument {
    // Two members whose ids name source issues issue-1 and issue-2.
    let member_a = MemberView {
        workpiece: WorkpieceId("issue-1".into()),
        scope_revision: digest(10),
        approval: approval(digest(10)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let member_b = MemberView {
        workpiece: WorkpieceId("issue-2".into()),
        scope_revision: digest(20),
        approval: approval(digest(20)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let bloom = BloomView {
        id: BloomId(digest(1)),
        status: BloomStatus::Sealed,
        superseded_by: None,
        members: vec![member_a, member_b],
        landing_blocked: None,
    };
    ViewDocument { mainline: digest(0), blooms: vec![bloom] }
}

fn landing_branch(bloom: BloomId) -> String {
    format!("bloom/{}/landing", aether_bloomery_github::short_hex(&bloom.0))
}

fn seed_pr(fake: &FakeGithub, bloom: BloomId) -> u64 {
    fake.create_pull_request(&NewPullRequest {
        title: "land".into(),
        body: "b".into(),
        head: landing_branch(bloom),
        base: "main".into(),
    })
    .expect("pr created")
    .number
}

#[test]
fn reconcile_creates_comments_on_source_issues_and_pr_without_creating_issues() {
    let fake = FakeGithub::new();
    fake.seed_issue(1, "source 1");
    fake.seed_issue(2, "source 2");
    let view = issuable_view();
    let pr = seed_pr(&fake, view.blooms[0].id);

    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view).expect("reconcile");

    assert_eq!(fake.create_issue_calls(), 0, "no shadow issue opened");
    assert_eq!(fake.find_issue_calls(), 0, "no list-and-scan walk");
    assert_eq!(fake.issue_count(), 2, "only seeded source issues exist");
    // Each member adds: member body + approval comment. So 2*2 =4 plus bloom aggregate on PR.
    assert!(fake.comment_count() >= 4, "member comments landed on source issues");
    // Bloom aggregate landed as comment on PR.
    assert!(fake.comment_count() >= 5, "bloom aggregate landed on PR");
    // Verify a comment landed on PR number.
    let pr_comments: Vec<_> = fake.comment_count().to_string().chars().collect();
    let _ = pr_comments;
    // At least one comment is on the PR issue number.
    // Fake stores comments with issue_number = pr number; we can check by looking at comments via find_comment.
    // Use direct check: bloom key comment should be on PR.
    let bloom_key = format!("bloom:{}", aether_bloomery_github::short_hex(&digest(1)));
    let found = fake.find_comment(pr, &bloom_key).expect("find").is_some();
    assert!(found, "bloom aggregate comment exists on PR");
}

#[test]
fn reconciling_the_same_document_twice_is_idempotent() {
    let fake = FakeGithub::new();
    fake.seed_issue(1, "s1");
    fake.seed_issue(2, "s2");
    let view = issuable_view();
    seed_pr(&fake, view.blooms[0].id);

    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view).expect("first");
    let after_first = fake.comment_count();
    projection.reconcile_view(&view).expect("second");
    assert_eq!(after_first, fake.comment_count(), "second reconcile is all no-ops");
    assert_eq!(fake.create_issue_calls(), 0);
    assert_eq!(fake.find_issue_calls(), 0);
}

#[test]
fn landing_receipt_lands_on_pr_and_source_issue_without_opening_issue() {
    let fake = FakeGithub::new();
    fake.seed_issue(1, "s1");
    fake.seed_issue(2, "s2");
    let view = issuable_view();
    let bloom_id = view.blooms[0].id;
    let pr = seed_pr(&fake, bloom_id);

    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view).expect("reconcile caches members");

    let receipt = LandingReceipt { bloom: bloom_id, previous_base: digest(0), new_head: digest(9) };
    projection.project_receipt(&receipt).expect("receipt");

    assert_eq!(fake.create_issue_calls(), 0, "receipt did not open a shadow issue");
    assert_eq!(fake.find_issue_calls(), 0, "receipt did not list issues");

    let receipt_key = format!("receipt:{}", aether_bloomery_github::short_hex(&bloom_id.0));
    assert!(fake.find_comment(pr, &receipt_key).unwrap().is_some(), "receipt comment on PR");
    assert!(fake.find_comment(1, &receipt_key).unwrap().is_some(), "receipt comment on source issue 1");
    assert!(fake.find_comment(2, &receipt_key).unwrap().is_some(), "receipt comment on source issue 2");
}

#[test]
fn non_issuable_workpiece_is_skipped_without_shadow_issue() {
    let fake = FakeGithub::new();
    fake.seed_issue(1, "s1");
    // View with one issuable and one non-issuable workpiece.
    let member_ok = MemberView {
        workpiece: WorkpieceId("issue-1".into()),
        scope_revision: digest(10),
        approval: approval(digest(10)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let member_bad = MemberView {
        workpiece: WorkpieceId("feature-foo".into()),
        scope_revision: digest(20),
        approval: approval(digest(20)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let bloom = BloomView {
        id: BloomId(digest(5)),
        status: BloomStatus::Sealed,
        superseded_by: None,
        members: vec![member_ok, member_bad],
        landing_blocked: None,
    };
    let view = ViewDocument { mainline: digest(0), blooms: vec![bloom] };
    seed_pr(&fake, BloomId(digest(5)));

    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view).expect("reconcile");

    assert_eq!(fake.create_issue_calls(), 0, "no shadow issue for non-issuable workpiece");
    assert_eq!(fake.find_issue_calls(), 0, "no list walk");
    assert_eq!(fake.issue_count(), 1, "only seeded source issue remains");
    // Comments only for the issuable member.
    assert!(fake.comment_count() >= 2, "issuable member projected");
    // No comment should exist for the non-issuable workpiece's key on any issue.
    let bloom_hex = aether_bloomery_github::short_hex(&digest(5));
    let bad_key = format!("wp:feature-foo@bloom:{bloom_hex}");
    // Check that bad key is absent everywhere (issue 1 has no bad key).
    assert!(fake.find_comment(1, &bad_key).unwrap().is_none(), "non-issuable member not projected");
}

#[test]
fn no_projection_path_uses_the_list_walk() {
    let fake = FakeGithub::new();
    fake.seed_issue(10, "s10");
    fake.seed_issue(11, "s11");
    let member_a = MemberView {
        workpiece: WorkpieceId("issue-10".into()),
        scope_revision: digest(10),
        approval: approval(digest(10)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let member_b = MemberView {
        workpiece: WorkpieceId("issue-11".into()),
        scope_revision: digest(11),
        approval: approval(digest(11)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let bloom = BloomView {
        id: BloomId(digest(9)),
        status: BloomStatus::Sealed,
        superseded_by: None,
        members: vec![member_a, member_b],
        landing_blocked: None,
    };
    let view = ViewDocument { mainline: digest(0), blooms: vec![bloom] };
    seed_pr(&fake, BloomId(digest(9)));

    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view).expect("reconcile");
    let receipt = LandingReceipt { bloom: BloomId(digest(9)), previous_base: digest(0), new_head: digest(99) };
    projection.project_receipt(&receipt).expect("receipt");

    assert_eq!(fake.find_issue_calls(), 0, "no projection path called find_issue");
    assert_eq!(fake.create_issue_calls(), 0, "no projection path opened an issue");
}

#[test]
fn missing_pr_is_skipped_without_shadow_issue() {
    let fake = FakeGithub::new();
    fake.seed_issue(20, "s20");
    let member = MemberView {
        workpiece: WorkpieceId("issue-20".into()),
        scope_revision: digest(20),
        approval: approval(digest(20)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let bloom = BloomView {
        id: BloomId(digest(20)),
        status: BloomStatus::Sealed,
        superseded_by: None,
        members: vec![member],
        landing_blocked: None,
    };
    let view = ViewDocument { mainline: digest(0), blooms: vec![bloom] };

    // Intentionally do NOT seed a PR.
    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view).expect("reconcile without PR");

    assert_eq!(fake.create_issue_calls(), 0, "missing PR does not create shadow issue");
    assert_eq!(fake.find_issue_calls(), 0);
    // Member still projects onto its source issue.
    assert!(fake.comment_count() >= 2, "member comments still land on source issue");
}

#[test]
fn workpiece_id_mapping_is_issue_dash_number() {
    assert_eq!(parse_source_issue_number(&WorkpieceId("issue-4628".into())), Some(4628));
    assert_eq!(parse_source_issue_number(&WorkpieceId("issue-1".into())), Some(1));
    assert_eq!(parse_source_issue_number(&WorkpieceId("issue-0".into())), None);
    assert_eq!(parse_source_issue_number(&WorkpieceId("issue-".into())), None);
    assert_eq!(parse_source_issue_number(&WorkpieceId("issue-abc".into())), None);
    assert_eq!(parse_source_issue_number(&WorkpieceId("feature-foo".into())), None);
    assert_eq!(parse_source_issue_number(&WorkpieceId("ISSUE-1".into())), None);
    assert_eq!(parse_source_issue_number(&WorkpieceId("issue-1-extra".into())), None);
}

#[test]
fn held_member_projects_question_comment_on_source_issue() {
    let fake = FakeGithub::new();
    fake.seed_issue(30, "s30");
    let bloom_id = BloomId(digest(30));
    seed_pr(&fake, bloom_id);

    let member = MemberView {
        workpiece: WorkpieceId("issue-30".into()),
        scope_revision: digest(30),
        approval: approval(digest(30)),
        resolution: None,
        pending_decision: Some(PendingDecisionView {
            question: digest(90),
            stage: StageId::Construct,
            prompt: "tie".into(),
            options: vec!["A".into(), "B".into()],
            blocked: "held".into(),
        }),
        wedge: None,
    };
    // Baseline view without hold to get baseline comment count.
    let base_view = ViewDocument {
        mainline: digest(0),
        blooms: vec![BloomView {
            id: bloom_id,
            status: BloomStatus::Sealed,
            superseded_by: None,
            members: vec![MemberView { pending_decision: None, ..member.clone() }],
            landing_blocked: None,
        }],
    };
    let held_view = ViewDocument {
        mainline: digest(0),
        blooms: vec![BloomView {
            id: bloom_id,
            status: BloomStatus::Sealed,
            superseded_by: None,
            members: vec![member],
            landing_blocked: None,
        }],
    };

    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&base_view).expect("base");
    let base_count = fake.comment_count();
    projection.reconcile_view(&held_view).expect("held");
    assert!(fake.comment_count() > base_count, "held question adds a comment on source issue");
    assert_eq!(fake.create_issue_calls(), 0);
    // Idempotent second held reconcile is no-op.
    let after = fake.comment_count();
    projection.reconcile_view(&held_view).expect("held again");
    assert_eq!(after, fake.comment_count());
}
