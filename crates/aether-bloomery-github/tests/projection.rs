//! Projection onto existing source issues and the landing pull request.
//! No shadow umbrella or workpiece issues are opened; the repo-wide
//! `find_issue` scan is never reached.

#![allow(clippy::unwrap_used)]

use std::cell::Cell;

use aether_bloomery::{
    BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, LandingReceipt, MemberView, PendingDecisionView,
    ProjectionBackend, ResolutionClaim, StageId, ViewDocument, WorkpieceId,
};
use aether_bloomery_github::{
    Comment, GithubApi, GithubError, GithubProjection, Issue, NewComment, NewIssue, NewPullRequest, PullRequest,
    PullRequestApi, short_hex, testing::FakeGithub,
};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn approval(subject: Digest) -> Evidence {
    Evidence { subject, kind: EvidenceKind::Approval, detail: digest(200) }
}

fn landing_branch(bloom: BloomId) -> String {
    format!("bloom/{}/landing", short_hex(&bloom.0))
}

/// Two-member bloom with source-issue workpieces (`issue-101`, `issue-102`).
fn view(resolve_second: bool) -> ViewDocument {
    let member_a = MemberView {
        workpiece: WorkpieceId("issue-101".into()),
        scope_revision: digest(10),
        approval: approval(digest(10)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let mut member_b = MemberView {
        workpiece: WorkpieceId("issue-102".into()),
        scope_revision: digest(20),
        approval: approval(digest(20)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    if resolve_second {
        member_b.resolution = Some(ResolutionClaim {
            workpiece: WorkpieceId("issue-102".into()),
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
fn reconcile_posts_comments_to_source_issues_without_opening_new_issues() {
    let fake = FakeGithub::new();
    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view(false)).expect("reconcile");

    assert_eq!(fake.issue_count(), 0, "no shadow issue is opened");
    // 1 approval comment per resolvable member, onto source issue numbers.
    assert_eq!(fake.comment_count(), 2);
}

#[test]
fn reconcile_with_missing_landing_pr_skips_bloom_comment_without_opening_issue() {
    let fake = FakeGithub::new();
    let projection = GithubProjection::new(fake.clone());
    // Bloom 1 has no landing PR yet (not seeded).
    projection.reconcile_view(&view(false)).expect("reconcile with no PR");

    assert_eq!(fake.issue_count(), 0, "no shadow umbrella when PR missing");
    // Member comments still post to source issues.
    assert_eq!(fake.comment_count(), 2);
}

#[test]
fn reconcile_posts_bloom_comment_to_landing_pr_when_it_exists() {
    let fake = FakeGithub::new();
    // Seed landing PR for bloom 1.
    let branch = landing_branch(BloomId(digest(1)));
    fake.seed_ref(&format!("heads/{branch}"), "abc123");
    fake.create_pull_request(&NewPullRequest {
        title: "land".into(),
        body: "".into(),
        head: branch.clone(),
        base: "main".into(),
    })
    .unwrap();

    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view(false)).expect("reconcile");

    assert_eq!(fake.issue_count(), 0);
    // 2 approval comments on source issues + 1 bloom comment on landing PR.
    assert_eq!(fake.comment_count(), 3);
}

#[test]
fn reconciling_the_same_document_twice_is_idempotent() {
    let fake = FakeGithub::new();
    let branch = landing_branch(BloomId(digest(1)));
    fake.seed_ref(&format!("heads/{branch}"), "abc123");
    fake.create_pull_request(&NewPullRequest {
        title: "land".into(),
        body: "".into(),
        head: branch,
        base: "main".into(),
    })
    .unwrap();
    let projection = GithubProjection::new(fake.clone());
    let document = view(false);

    projection.reconcile_view(&document).expect("first reconcile");
    let after_first = fake.comment_count();

    projection.reconcile_view(&document).expect("second reconcile");
    assert_eq!(fake.comment_count(), after_first, "second reconcile is no-op via marker digests");
    assert_eq!(fake.issue_count(), 0);
}

#[test]
fn a_held_member_projects_idempotent_question_comment_to_source_issue() {
    let fake = FakeGithub::new();
    let projection = GithubProjection::new(fake.clone());

    projection.reconcile_view(&view(false)).expect("baseline reconcile");
    assert_eq!(fake.comment_count(), 2);

    projection.reconcile_view(&held_view()).expect("held reconcile");
    assert_eq!(fake.comment_count(), 3, "the held member's question projects one comment");

    projection.reconcile_view(&held_view()).expect("idempotent reconcile");
    assert_eq!(fake.comment_count(), 3, "re-reconciling the same hold is a no-op");
    assert_eq!(fake.issue_count(), 0);
}

#[test]
fn workpiece_id_not_resolving_to_issue_is_skipped_without_shadow_issue() {
    let fake = FakeGithub::new();
    let projection = GithubProjection::new(fake.clone());

    // Workpiece "reactor-core" does not match `issue-<digits>` — no source home.
    let member = MemberView {
        workpiece: WorkpieceId("reactor-core".into()),
        scope_revision: digest(10),
        approval: approval(digest(10)),
        resolution: None,
        pending_decision: None,
        wedge: None,
    };
    let bloom = BloomView {
        id: BloomId(digest(9)),
        status: BloomStatus::Sealed,
        superseded_by: None,
        members: vec![member],
        landing_blocked: None,
    };
    let document = ViewDocument { mainline: digest(0), blooms: vec![bloom] };

    projection.reconcile_view(&document).expect("reconcile with unresolvable workpiece");

    assert_eq!(fake.issue_count(), 0, "no shadow issue for unresolvable workpiece");
    assert_eq!(fake.comment_count(), 0, "no comment when no source issue home");
}

#[test]
fn landing_receipt_lands_as_comment_on_pr_and_source_issue_with_no_new_issue() {
    let fake = FakeGithub::new();
    let bloom = BloomId(digest(1));
    let branch = landing_branch(bloom);
    fake.seed_ref(&format!("heads/{branch}"), "deadbeef");
    fake.create_pull_request(&NewPullRequest {
        title: "land".into(),
        body: "".into(),
        head: branch.clone(),
        base: "main".into(),
    })
    .unwrap();
    let pr_number = fake.find_pull_request_for_head(&branch).unwrap().unwrap().number;

    let projection = GithubProjection::new(fake.clone());
    // Populate bloom→members cache so receipt can fan out to source issues.
    projection.reconcile_view(&view(false)).expect("reconcile to populate cache");
    let comments_before_receipt = fake.comment_count();

    let receipt = LandingReceipt { bloom, previous_base: digest(10), new_head: digest(20) };
    projection.project_receipt(&receipt).expect("project receipt");

    assert_eq!(fake.issue_count(), 0, "receipt opens no issue");

    // Receipt posts to landing PR...
    let pr_comments: Vec<_> =
        fake.comments_for_issue(pr_number).into_iter().filter(|comment| comment.body.contains("Landed")).collect();
    assert_eq!(pr_comments.len(), 1, "receipt posts one comment on landing PR");

    // ...and to each resolvable source issue (101, 102).
    for source in [101u64, 102u64] {
        let source_comments: Vec<_> =
            fake.comments_for_issue(source).into_iter().filter(|comment| comment.body.contains("Landed")).collect();
        assert_eq!(source_comments.len(), 1, "receipt posts one comment on source issue {source}");
    }
    // Idempotent: second receipt does not duplicate.
    projection.project_receipt(&receipt).expect("second receipt");
    assert_eq!(fake.comment_count(), comments_before_receipt + 3, "second receipt is no-op");
}

/// Wrapper that counts `find_issue` / `create_issue` calls — the old
/// repository-wide scan must never be reached from a projection.
struct CountingGithub {
    inner: FakeGithub,
    find_issue_calls: Cell<usize>,
    create_issue_calls: Cell<usize>,
}

impl CountingGithub {
    fn new(inner: FakeGithub) -> Self {
        Self { inner, find_issue_calls: Cell::new(0), create_issue_calls: Cell::new(0) }
    }

    fn issue_count(&self) -> usize {
        self.inner.issue_count()
    }

    fn comment_count(&self) -> usize {
        self.inner.comment_count()
    }
}

impl GithubApi for CountingGithub {
    fn find_issue(&self, key: &str) -> Result<Option<Issue>, GithubError> {
        self.find_issue_calls.set(self.find_issue_calls.get() + 1);
        self.inner.find_issue(key)
    }
    fn create_issue(&self, new: &NewIssue) -> Result<Issue, GithubError> {
        self.create_issue_calls.set(self.create_issue_calls.get() + 1);
        self.inner.create_issue(new)
    }
    fn update_issue(&self, number: u64, title: &str, body: &str) -> Result<(), GithubError> {
        self.inner.update_issue(number, title, body)
    }
    fn find_comment(&self, issue_number: u64, key: &str) -> Result<Option<Comment>, GithubError> {
        self.inner.find_comment(issue_number, key)
    }
    fn create_comment(&self, new: &NewComment) -> Result<Comment, GithubError> {
        self.inner.create_comment(new)
    }
    fn update_comment(&self, comment_id: u64, body: &str) -> Result<(), GithubError> {
        self.inner.update_comment(comment_id, body)
    }
}

impl PullRequestApi for CountingGithub {
    fn create_pull_request(&self, new: &NewPullRequest) -> Result<PullRequest, GithubError> {
        self.inner.create_pull_request(new)
    }
    fn get_pull_request(&self, number: u64) -> Result<Option<PullRequest>, GithubError> {
        self.inner.get_pull_request(number)
    }
    fn find_pull_request_for_head(&self, head: &str) -> Result<Option<PullRequest>, GithubError> {
        self.inner.find_pull_request_for_head(head)
    }
    fn checks_for_ref(&self, sha: &str) -> Result<aether_bloomery_github::ChecksState, GithubError> {
        self.inner.checks_for_ref(sha)
    }
}

#[test]
fn no_projection_path_calls_repository_wide_issue_list() {
    let inner = FakeGithub::new();
    let branch = landing_branch(BloomId(digest(1)));
    inner.seed_ref(&format!("heads/{branch}"), "abc");
    inner
        .create_pull_request(&NewPullRequest {
            title: "land".into(),
            body: "".into(),
            head: branch,
            base: "main".into(),
        })
        .unwrap();

    let counting = CountingGithub::new(inner);
    let projection = GithubProjection::new(counting);
    // Both projection entrypoints must avoid `find_issue`.
    projection.reconcile_view(&view(false)).expect("reconcile");
    let receipt = LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(10), new_head: digest(20) };
    projection.project_receipt(&receipt).expect("receipt");

    // Would fail if old behaviour returned and scanned the repo issue list.
    assert_eq!(projection.client().find_issue_calls.get(), 0, "no projection path walks the repo issue list");
    assert_eq!(projection.client().create_issue_calls.get(), 0, "no projection path opens an issue");
}

#[test]
fn no_projection_path_opens_an_issue() {
    let inner = FakeGithub::new();
    let branch = landing_branch(BloomId(digest(1)));
    inner.seed_ref(&format!("heads/{branch}"), "abc");
    inner
        .create_pull_request(&NewPullRequest {
            title: "land".into(),
            body: "".into(),
            head: branch,
            base: "main".into(),
        })
        .unwrap();
    let counting = CountingGithub::new(inner);
    let projection = GithubProjection::new(counting);
    projection.reconcile_view(&view(true)).expect("reconcile");
    projection.reconcile_view(&held_view()).expect("held view");
    let receipt = LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(10), new_head: digest(20) };
    projection.project_receipt(&receipt).expect("receipt");
    assert_eq!(projection.client().create_issue_calls.get(), 0, "no projection path opens an issue");
}
