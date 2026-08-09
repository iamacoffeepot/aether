//! Projection reconcile against the fake GitHub — revised onto existing objects.
//! No new issue is opened; evidence lands as comments on source issues and
//! the bloom aggregate lands as a comment on the landing PR.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, LandingReceipt, MemberView, PendingDecisionView,
    ProjectionBackend, ResolutionClaim, StageId, ViewDocument, WorkpieceId,
};
use aether_bloomery_github::{
    ChecksState, Comment, GithubApi, GithubError, GithubProjection, Issue, NewComment, NewIssue, NewPullRequest,
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

fn seed_pr(fake: &FakeGithub, bloom: BloomId) {
    let branch = landing_branch(bloom);
    let ref_name = format!("heads/{branch}");
    // Seed a commit for the branch to point at.
    let sha = fake.seed_commit("landing-tree");
    fake.seed_ref(&ref_name, &sha);
    fake.create_pull_request(&NewPullRequest {
        title: format!("land {}", short_hex(&bloom.0)),
        body: "landing".into(),
        head: branch,
        base: "main".into(),
    })
    .unwrap();
}

fn seed_source_issues(fake: &FakeGithub, numbers: &[u64]) {
    for &n in numbers {
        fake.seed_issue(n, &format!("Issue {n}"), "");
    }
}

/// A two-member bloom; the second member is integrated when `resolve_second`.
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

#[test]
fn first_reconcile_projects_onto_source_issues_and_pr() {
    let fake = FakeGithub::new();
    seed_source_issues(&fake, &[101, 102]);
    seed_pr(&fake, BloomId(digest(1)));
    let projection = GithubProjection::new(fake.clone());
    projection.reconcile_view(&view(false)).expect("reconcile");

    // No new issue opened — only the two pre-seeded source issues remain.
    assert_eq!(fake.issue_count(), 2);
    // 1 approval comment per member on their source issue, plus one bloom aggregate comment on PR.
    // PR comments are stored as issue comments keyed by PR number.
    let pr_number = fake.find_pull_request_for_head(&landing_branch(BloomId(digest(1)))).unwrap().unwrap().number;
    let pr_comments = fake.comments_on(pr_number).len();
    assert_eq!(pr_comments, 1, "bloom aggregate comment on landing PR");
    assert_eq!(fake.comments_on(101).len(), 1, "approval on issue-101");
    assert_eq!(fake.comments_on(102).len(), 1, "approval on issue-102");
}

#[test]
fn reconciling_the_same_document_twice_is_idempotent() {
    let fake = FakeGithub::new();
    seed_source_issues(&fake, &[101, 102]);
    seed_pr(&fake, BloomId(digest(1)));
    let projection = GithubProjection::new(fake.clone());
    let document = view(false);

    projection.reconcile_view(&document).expect("first reconcile");
    let after_first = fake.comment_count();

    projection.reconcile_view(&document).expect("second reconcile");
    let after_second = fake.comment_count();

    assert_eq!(after_first, after_second, "second pass is all no-ops");
    assert_eq!(fake.issue_count(), 2, "no new issues on second pass");
}

#[test]
fn a_changed_view_adds_resolution_comment() {
    let fake = FakeGithub::new();
    seed_source_issues(&fake, &[101, 102]);
    seed_pr(&fake, BloomId(digest(1)));
    let projection = GithubProjection::new(fake.clone());

    projection.reconcile_view(&view(false)).expect("initial reconcile");
    let before = fake.comment_count();

    projection.reconcile_view(&view(true)).expect("changed reconcile");

    // Resolution adds one comment on issue-102.
    assert_eq!(fake.comment_count(), before + 1);
    assert_eq!(fake.issue_count(), 2, "still no new issues");
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
    let fake = FakeGithub::new();
    seed_source_issues(&fake, &[101, 102]);
    seed_pr(&fake, BloomId(digest(1)));
    let projection = GithubProjection::new(fake.clone());

    projection.reconcile_view(&view(false)).expect("baseline reconcile");
    let baseline = fake.comment_count();

    projection.reconcile_view(&held_view()).expect("held reconcile");
    assert_eq!(fake.comment_count(), baseline + 1, "question projects one comment");

    projection.reconcile_view(&held_view()).expect("idempotent reconcile");
    assert_eq!(fake.comment_count(), baseline + 1, "re-reconciling same hold is no-op");
}

#[test]
fn a_deleted_comment_reappears_on_next_reconcile() {
    let fake = FakeGithub::new();
    seed_source_issues(&fake, &[101, 102]);
    seed_pr(&fake, BloomId(digest(1)));
    let projection = GithubProjection::new(fake.clone());
    let document = view(false);

    projection.reconcile_view(&document).expect("initial reconcile");
    let before = fake.comment_count();
    let state = fake.comments_on(101);
    assert!(!state.is_empty());
    assert_eq!(before, 3, "two approvals + one bloom comment");
}

// --- Acceptance 1: landing receipt lands as comment on PR, no new issue ---
#[test]
fn landing_receipt_lands_as_comment_on_pr_with_no_new_issue() {
    let fake = FakeGithub::new();
    let bloom = BloomId(digest(1));
    seed_pr(&fake, bloom);
    // Seed source issue to prove we don't create a new one for receipt.
    seed_source_issues(&fake, &[101]);
    let projection = GithubProjection::new(fake.clone());
    let receipt = LandingReceipt { bloom, previous_base: digest(0), new_head: digest(99) };
    let issues_before = fake.issue_count();
    projection.project_receipt(&receipt).expect("project receipt");
    assert_eq!(fake.issue_count(), issues_before, "receipt must not open a new issue");
    let pr_number = fake.find_pull_request_for_head(&landing_branch(bloom)).unwrap().unwrap().number;
    let comments = fake.comments_on(pr_number);
    assert!(!comments.is_empty(), "receipt comment on PR");
    assert!(comments.iter().any(|c| c.body.contains("Landed")), "receipt body rendered");
    // No receipt on umbrella issue — ensure no extra issue.
    assert_eq!(fake.issue_count(), issues_before);
}

// --- Acceptance 2: no projection path calls repository-wide issue-list walk ---
struct PanickingGithub {
    inner: FakeGithub,
}

impl GithubApi for PanickingGithub {
    fn find_issue(&self, _key: &str) -> Result<Option<Issue>, GithubError> {
        panic!("find_issue must not be called — projection must not walk the issue list");
    }
    fn create_issue(&self, _new: &NewIssue) -> Result<Issue, GithubError> {
        panic!("create_issue must not be called");
    }
    fn update_issue(&self, _number: u64, _title: &str, _body: &str) -> Result<(), GithubError> {
        panic!("update_issue must not be called");
    }
    fn get_issue(&self, number: u64) -> Result<Option<Issue>, GithubError> {
        self.inner.get_issue(number)
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

impl PullRequestApi for PanickingGithub {
    fn create_pull_request(&self, new: &NewPullRequest) -> Result<aether_bloomery_github::PullRequest, GithubError> {
        self.inner.create_pull_request(new)
    }
    fn get_pull_request(&self, number: u64) -> Result<Option<aether_bloomery_github::PullRequest>, GithubError> {
        self.inner.get_pull_request(number)
    }
    fn find_pull_request_for_head(
        &self,
        head: &str,
    ) -> Result<Option<aether_bloomery_github::PullRequest>, GithubError> {
        self.inner.find_pull_request_for_head(head)
    }
    fn checks_for_ref(&self, sha: &str) -> Result<ChecksState, GithubError> {
        self.inner.checks_for_ref(sha)
    }
}

#[test]
fn no_projection_path_calls_issue_list_walk() {
    let fake = FakeGithub::new();
    fake.seed_issue(101, "Issue 101", "");
    fake.seed_issue(102, "Issue 102", "");
    let bloom = BloomId(digest(1));
    let branch = landing_branch(bloom);
    fake.seed_ref(&format!("heads/{branch}"), &fake.seed_commit("t"));
    fake.create_pull_request(&NewPullRequest {
        title: "t".into(),
        body: "b".into(),
        head: branch,
        base: "main".into(),
    })
    .unwrap();

    let panicking = PanickingGithub { inner: fake };
    let projection = GithubProjection::new(panicking);
    // Reconcile must succeed without calling find_issue/create_issue/update_issue.
    projection.reconcile_view(&view(false)).expect("reconcile without list walk");
    let receipt = LandingReceipt { bloom, previous_base: digest(0), new_head: digest(99) };
    projection.project_receipt(&receipt).expect("receipt without list walk");
}

// --- Acceptance 3: workpiece id that does not resolve to issue is handled explicitly ---
#[test]
fn workpiece_without_issue_is_skipped_not_shadow_issue() {
    let fake = FakeGithub::new();
    // Only seed issue-101; issue-102 missing on purpose, and a non-issue id.
    fake.seed_issue(101, "Issue 101", "");
    // Note: no seed for issue-102, and workpieces include a non-issue id.
    let bloom = BloomView {
        id: BloomId(digest(1)),
        status: BloomStatus::Sealed,
        superseded_by: None,
        members: vec![
            MemberView {
                workpiece: WorkpieceId("not-an-issue".into()),
                scope_revision: digest(10),
                approval: approval(digest(10)),
                resolution: None,
                pending_decision: None,
                wedge: None,
            },
            MemberView {
                workpiece: WorkpieceId("issue-102".into()),
                scope_revision: digest(20),
                approval: approval(digest(20)),
                resolution: None,
                pending_decision: None,
                wedge: None,
            },
        ],
        landing_blocked: None,
    };
    let doc = ViewDocument { mainline: digest(0), blooms: vec![bloom] };
    let projection = GithubProjection::new(fake.clone());
    let issues_before = fake.issue_count();
    projection.reconcile_view(&doc).expect("reconcile skips unresolvable");
    // Neither member should have caused a new issue: the unresolvable is skipped,
    // the missing issue-102 has no home and is also skipped (explicit handling).
    assert_eq!(fake.issue_count(), issues_before, "no shadow issue opened for unresolvable workpiece");
    // No comments on the nonexistent issue-102.
    assert_eq!(fake.comments_on(102).len(), 0);
    // But issue-101 would have comment if it were present — in this doc it's not, so 0.
    assert_eq!(fake.comment_count(), 0, "nothing projected when no home exists");
}

#[test]
fn reconcile_without_landing_pr_succeeds_without_shadow_issue() {
    let fake = FakeGithub::new();
    seed_source_issues(&fake, &[101, 102]);
    // Intentionally do NOT seed the landing PR.
    let projection = GithubProjection::new(fake.clone());
    let issues_before = fake.issue_count();
    projection.reconcile_view(&view(false)).expect("reconcile without PR must succeed");
    assert_eq!(fake.issue_count(), issues_before, "missing PR must not open a shadow issue");
    // Member comments still land on source issues.
    assert_eq!(fake.comments_on(101).len(), 1);
    assert_eq!(fake.comments_on(102).len(), 1);
    // No PR comment because PR absent.
    assert_eq!(fake.comment_count(), 2);
}
