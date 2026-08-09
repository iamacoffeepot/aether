//! Projection reconcile against the fake GitHub: the new source-issue + landing-PR
//! mapping, idempotency, and the three acceptance properties.
//!
//! The shadow-issue mirror (umbrella + workpiece issues found via repository-wide
//! `find_issue` scan) is retired. The projection now comments on the existing
//! source issue (`issue-<N>` → issue N) and the landing pull request
//! (`bloom/<short>/landing`), never opening a new issue and never scanning the
//! repository issue list.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, LandingReceipt, MemberView, PendingDecisionView,
    ProjectionBackend, ResolutionClaim, StageId, ViewDocument, WorkpieceId,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{GithubProjection, NewPullRequest, PullRequestApi, short_hex};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn approval(subject: Digest) -> Evidence {
    Evidence { subject, kind: EvidenceKind::Approval, detail: digest(200) }
}

fn landing_branch(bloom: BloomId) -> String {
    format!("bloom/{}/landing", short_hex(&bloom.0))
}

fn seed_landing_pr(fake: &FakeGithub, bloom: BloomId) {
    let branch = landing_branch(bloom);
    // Seed the ref the PR will point at so the fake has a head sha, then open PR.
    let sha = fake.seed_commit("tree");
    fake.seed_ref(&format!("heads/{branch}"), &sha);
    fake.create_pull_request(&NewPullRequest {
        title: format!("land {}", short_hex(&bloom.0)),
        body: String::new(),
        head: branch,
        base: "main".into(),
    })
    .unwrap();
}

/// A two-member bloom whose workpieces name source issues.
fn view_with_issues(resolve_second: bool) -> ViewDocument {
    let member_a = MemberView {
        workpiece: WorkpieceId("issue-4628".into()),
        scope_revision: digest(10),
        approval: approval(digest(10)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let mut member_b = MemberView {
        workpiece: WorkpieceId("issue-4629".into()),
        scope_revision: digest(20),
        approval: approval(digest(20)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    if resolve_second {
        member_b.resolution = Some(ResolutionClaim {
            workpiece: WorkpieceId("issue-4629".into()),
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
        landing_blocked: None,
    };
    ViewDocument { mainline: digest(0), blooms: vec![bloom] }
}

#[test]
fn reconcile_does_not_open_new_issues() {
    // Acceptance 1 & 2: no projection path opens an issue, and the reconcile
    // lands comments on the existing source issues and landing PR rather than
    // shadow issues.
    let fake = FakeGithub::new();
    fake.seed_issue(4628, "source issue 4628");
    fake.seed_issue(4629, "source issue 4629");
    seed_landing_pr(&fake, BloomId(digest(1)));

    let projection = GithubProjection::new(fake.clone());
    let before = fake.issue_count();
    projection.reconcile_view(&view_with_issues(false)).expect("reconcile");

    assert_eq!(
        fake.issue_count(),
        before,
        "no new issue opened — old behaviour would have opened umbrella + workpiece issues"
    );
    // 2 approval comments + 1 bloom aggregate comment on the PR
    assert_eq!(fake.comment_count(), 3, "approval comments on source issues + bloom comment on landing PR");
    // Verify comments landed where expected
    assert!(!fake.comments_for(4628).is_empty(), "issue-4628 got its approval comment");
    assert!(!fake.comments_for(4629).is_empty(), "issue-4629 got its approval comment");
    // The landing PR holds the bloom aggregate comment
    let pr_number = fake.find_pull_request_for_head(&landing_branch(BloomId(digest(1)))).unwrap().unwrap().number;
    assert!(!fake.comments_for(pr_number).is_empty(), "landing PR got bloom aggregate comment");
}

#[test]
fn no_projection_path_calls_the_repository_wide_issue_list_scan() {
    // Acceptance 2: `find_issue` is the repository-wide list-and-scan (47 requests,
    // growing forever). No projection path should call it.
    let fake = FakeGithub::new();
    fake.seed_issue(4628, "source");
    fake.seed_issue(4629, "source");
    seed_landing_pr(&fake, BloomId(digest(1)));

    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view_with_issues(false)).expect("reconcile");
    projection
        .project_receipt(&LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(10), new_head: digest(20) })
        .expect("receipt");

    assert_eq!(
        fake.find_issue_calls(),
        0,
        "list-and-scan is not reachable from any projection path — old code called find_issue"
    );
}

#[test]
fn workpiece_without_issue_prefix_is_skipped_explicitly() {
    // Acceptance 3: a workpiece id that does not resolve to an issue is handled
    // explicitly rather than falling back to opening a shadow issue.
    let fake = FakeGithub::new();
    fake.seed_issue(4628, "source 4628");
    seed_landing_pr(&fake, BloomId(digest(1)));

    let member_a = MemberView {
        workpiece: WorkpieceId("feature-xyz".into()),
        scope_revision: digest(10),
        approval: approval(digest(10)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let member_b = MemberView {
        workpiece: WorkpieceId("issue-4628".into()),
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
    let view = ViewDocument { mainline: digest(0), blooms: vec![bloom] };

    let projection = GithubProjection::new(fake.clone());
    let before_issues = fake.issue_count();
    let before_comments = fake.comment_count();
    projection.reconcile_view(&view).expect("reconcile");

    assert_eq!(
        fake.issue_count(),
        before_issues,
        "no shadow issue opened for non-issue workpiece — old code would have"
    );
    // Only the resolvable member got a comment; the unresolvable one was skipped.
    assert_eq!(
        fake.comment_count(),
        before_comments + 2,
        "one bloom aggregate on PR + one approval on 4628; feature-xyz skipped"
    );
    // No comment was created for the unresolvable workpiece's fake issue number
    assert!(
        fake.comments_for(4628).iter().any(|c| c.contains("4628") || c.contains("Evidence")),
        "source issue 4628 still got its comment"
    );
}

#[test]
fn landing_receipt_lands_on_pr_and_source_issue_without_new_issue() {
    let fake = FakeGithub::new();
    fake.seed_issue(4628, "source 4628");
    fake.seed_issue(4629, "source 4629");
    seed_landing_pr(&fake, BloomId(digest(1)));

    // Reconcile to populate receipt cache (so receipt knows which source issues to fan out to)
    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view_with_issues(false)).expect("reconcile");
    let before_issues = fake.issue_count();
    let before_comments = fake.comment_count();

    projection
        .project_receipt(&LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(10), new_head: digest(20) })
        .expect("receipt");

    assert_eq!(fake.issue_count(), before_issues, "receipt opened no new issue — old code would have opened umbrella");
    // One receipt comment on PR + one per resolvable source issue (4628, 4629)
    assert_eq!(fake.comment_count(), before_comments + 3, "receipt fanned to PR and each source issue");

    let pr_number = fake.find_pull_request_for_head(&landing_branch(BloomId(digest(1)))).unwrap().unwrap().number;
    assert!(fake.comments_for(pr_number).iter().any(|c| c.contains("Landed")), "PR got receipt comment");
    assert!(fake.comments_for(4628).iter().any(|c| c.contains("Landed")), "source issue 4628 got receipt copy");
    assert!(fake.comments_for(4629).iter().any(|c| c.contains("Landed")), "source issue 4629 got receipt copy");
    assert_eq!(fake.find_issue_calls(), 0, "receipt also does not call list-and-scan");
}

#[test]
fn missing_landing_pr_is_skipped_without_shadow_issue() {
    // The landing PR may not exist yet when the first view document projects.
    // That is handled explicitly: no shadow issue, no error.
    let fake = FakeGithub::new();
    fake.seed_issue(4628, "source");
    fake.seed_issue(4629, "source");
    // Intentionally do not seed the landing PR.

    let projection = GithubProjection::new(fake.clone());
    let before = fake.issue_count();
    projection.reconcile_view(&view_with_issues(false)).expect("reconcile without PR");

    assert_eq!(fake.issue_count(), before, "missing PR does not fall back to opening a shadow issue");
    // Member evidence still lands on source issues even though bloom aggregate was skipped.
    assert_eq!(fake.comment_count(), 2, "approvals still land on source issues");
    assert_eq!(fake.find_issue_calls(), 0);
}

#[test]
fn reconciling_the_same_document_twice_is_idempotent() {
    let fake = FakeGithub::new();
    fake.seed_issue(4628, "source");
    fake.seed_issue(4629, "source");
    seed_landing_pr(&fake, BloomId(digest(1)));

    let projection = GithubProjection::new(fake.clone());
    let doc = view_with_issues(false);
    projection.reconcile_view(&doc).expect("first");
    let after_first = fake.comment_count();
    projection.reconcile_view(&doc).expect("second");
    assert_eq!(fake.comment_count(), after_first, "second reconcile is no-op via marker digests");
    assert_eq!(fake.find_issue_calls(), 0);
}

#[test]
fn a_held_member_projects_an_idempotent_question_comment() {
    let fake = FakeGithub::new();
    fake.seed_issue(4628, "source");
    fake.seed_issue(4629, "source");
    seed_landing_pr(&fake, BloomId(digest(1)));

    let projection = GithubProjection::new(fake.clone());
    // Baseline
    projection.reconcile_view(&view_with_issues(false)).expect("baseline");
    assert_eq!(fake.comment_count(), 3);

    let mut held = view_with_issues(false);
    held.blooms[0].members[0].pending_decision = Some(PendingDecisionView {
        question: digest(90),
        stage: StageId::Construct,
        prompt: "tie between A and B".into(),
        options: vec!["A".into(), "B".into()],
        blocked: "construct is held".into(),
    });
    projection.reconcile_view(&held).expect("held");
    assert_eq!(fake.comment_count(), 4, "question comment added on source issue 4628");

    projection.reconcile_view(&held).expect("idempotent");
    assert_eq!(fake.comment_count(), 4, "re-reconciling same hold is no-op");
}
