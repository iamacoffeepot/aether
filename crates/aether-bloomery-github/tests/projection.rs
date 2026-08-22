//! Projection reconcile against the fake GitHub (#3459 step 5 coverage, as
//! narrowed by #4663): the objects the projection must leave alone, the one
//! folded comment per member it writes, idempotency, in-place update, the
//! receipt's comment on the landing pull request, and the
//! delete → reappear rebuild property.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    BloomId, BloomStatus, BloomView, CommissionProjection, Digest, Evidence, EvidenceKind, LandingReceipt, MemberView,
    PendingDecisionView, ProjectedReceipt, ProjectionBackend, ResolutionClaim, StageId, ViewDocument, WithdrawnView,
    WorkpieceId,
};
use aether_bloomery_github::{
    CommissionProjectionApi, GithubProjection, Marker, NewIssue, commission_floor_title, issue_title_is_valid,
    landing_branch, marker::render_marker, testing::FakeGithub,
};

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
        cursor: None,
        park: None,
        awaiting_surface: None,
        withdrawn: None,
        leases: Vec::new(),
        evicted_by: None,
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
        operator_hold: None,
        blocker: None,
        leases: Vec::new(),
    };
    ViewDocument {
        mainline: digest(0),
        observed: digest(0),
        spend_quiesce: None,
        blooms: vec![bloom],
        base_alert: None,
    }
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
fn a_withdrawn_member_does_not_render_as_in_progress() {
    // Tripwire: member_state used to fall through to "in progress" for a
    // withdrawn member, so turning the view producer on would publish a
    // comment that is wrong rather than absent.
    let projection = GithubProjection::new(seeded());
    let mut document = view(false);
    document.blooms[0].members[0].withdrawn = Some(WithdrawnView {
        cause: "operator".into(),
        depends_on: None,
        reason: "dropped from the bloom".into(),
        operator: "eve".into(),
    });

    projection.reconcile_view(&document).expect("withdrawn reconcile");
    let body = &projection.client().comments_on(MEMBER_A)[0];
    assert!(body.contains("- State: withdrawn"), "a withdrawal is a terminal state, not silence: {body}");
    assert!(!body.contains("in progress"), "a withdrawn member must not read as still working: {body}");
    assert!(
        body.contains("**Withdrawn** (operator): dropped from the bloom — operator `eve`."),
        "the comment names cause, reason, and operator: {body}",
    );
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
fn a_receipt_reaches_the_landing_pull_request() {
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

    assert!(
        projection.client().comments_on(MEMBER_A).is_empty(),
        "the member's landing comment is written by the land reactor"
    );
    assert!(
        projection.client().comments_on(MEMBER_B).is_empty(),
        "the member's landing comment is written by the land reactor"
    );
    assert_eq!(projection.client().comments_on(LANDING).len(), 1, "the landing pull request is a target");
    assert_eq!(projection.client().issue_count(), 2, "a receipt opens nothing");
}

#[test]
fn a_receipt_with_no_landing_pull_request_comments_on_nothing() {
    // The pull request is a target, not a precondition: a bloom can land through
    // a path that opened none, and requiring one would wedge those lanes.
    // Member issues are closed by the land reactor, not this projection.
    let projection = GithubProjection::new(seeded());

    projection
        .project_receipt(&ProjectedReceipt {
            receipt: LandingReceipt { bloom: BloomId(digest(1)), previous_base: digest(0), new_head: digest(40) },
            members: vec![WorkpieceId(format!("issue-{MEMBER_A}"))],
        })
        .expect("a bloom that opened no proposal still receipts");

    assert!(
        projection.client().comments_on(MEMBER_A).is_empty(),
        "the member's landing comment is written by the land reactor"
    );
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

fn commission(workpiece: &str, recorded_issue: Option<u64>) -> CommissionProjection {
    CommissionProjection {
        workpiece: WorkpieceId(workpiece.to_owned()),
        intent: digest(1),
        scope_revision: Some(digest(2)),
        approval_signer: Some("operator".to_owned()),
        approval_digest: Some(digest(3)),
        status: "open".to_owned(),
        recorded_issue,
        title: String::new(),
    }
}

#[test]
fn reconciling_a_commission_twice_creates_one_issue() {
    // Idempotency is find-by-marker then recorded number. A second pass that
    // created again would duplicate the replica; a pass that forgot the
    // marker would do the same after a crash between create and persist.
    let projection = GithubProjection::new(FakeGithub::new());
    let first = projection.project_commission(&commission("wp-1", None)).expect("first create");
    let second = projection.project_commission(&commission("wp-1", None)).expect("second reconcile");

    assert_eq!(first, second, "the second pass must reuse the created number");
    assert_eq!(projection.client().issue_count(), 1, "reconciling twice creates one issue");
    let recorded = projection.project_commission(&commission("wp-1", first)).expect("recorded reconcile");
    assert_eq!(recorded, first);
    assert_eq!(projection.client().issue_count(), 1);
}

#[test]
fn a_human_edit_of_a_replica_is_overwritten_and_never_read() {
    // The plausible bug: reading the live GitHub title/body as input, or
    // skipping the overwrite when the marker still matches the pre-edit
    // digest, would leave operator prose as the replica.
    let projection = GithubProjection::new(FakeGithub::new());
    let number = projection.project_commission(&commission("wp-1", None)).expect("create").expect("owns a replica");
    projection.client().edit_issue(number, "a person renamed this", "a person rewrote the body");

    projection.project_commission(&commission("wp-1", Some(number))).expect("overwrite");

    let title = projection.client().issue_title(number).expect("the replica still exists");
    let body = projection.client().issue_body(number).expect("the replica still exists");
    assert_eq!(title, commission_floor_title("wp-1"), "an untitled replica is retitled to the floor: {title}");
    assert!(!title.contains("a person renamed this"), "the human title is not kept: {title}");
    assert!(body.contains("do not edit"), "the replica notice is restored: {body}");
    assert!(!body.contains("a person rewrote the body"), "the human body is not read: {body}");
    assert!(body.contains("operator"), "the approval signer is rendered: {body}");
}

#[test]
fn the_projector_will_not_write_title_or_body_to_an_issue_it_did_not_create() {
    // Adoption is the hole the 2026-08-16 amendment closes. A workpiece that
    // looks like issue-42 must not retitle the human object already numbered
    // 42. The projection surface for a home it does not own is a comment.
    let fake = FakeGithub::new();
    fake.seed_issue_with_title(42, "human title", "human body");
    let projection = GithubProjection::new(fake);

    let created = projection.project_commission(&commission("issue-42", None)).expect("project onto the named issue");

    assert_eq!(created, None, "the projector owns no issue for a named object");
    assert_eq!(projection.client().issue_title(42).as_deref(), Some("human title"));
    assert_eq!(projection.client().issue_body(42).as_deref(), Some("human body"));
    assert_eq!(projection.client().issue_count(), 1, "no replica opens beside the human issue");
    let comments = projection.client().comments_on(42);
    assert_eq!(comments.len(), 1, "one marker-keyed comment carries the commission");
    assert!(comments[0].contains("`issue-42`"), "the comment names the workpiece: {}", comments[0]);
    assert!(comments[0].contains("operator"), "the comment carries commission state: {}", comments[0]);
    assert!(!comments[0].contains("do not edit"), "the replica preamble is false on a human issue: {}", comments[0]);
}

#[test]
fn a_landed_commission_closes_only_the_owned_replica() {
    // Close is best-effort lifecycle of the replica, not of a human issue the
    // workpiece id happens to resemble. A named object has no replica to close;
    // closing the human issue is the land reactor's job.
    let fake = FakeGithub::new();
    fake.seed_issue(7, "human");
    let projection = GithubProjection::new(fake);
    let mut landed = commission("issue-7", None);
    landed.status = "landed".to_owned();
    let created = projection.project_commission(&landed).expect("project onto the named issue");

    assert_eq!(created, None, "a named object owns no replica");
    assert_eq!(projection.client().issue_is_closed(7), Some(false), "the human issue stays open");
    assert_eq!(projection.client().issue_count(), 1, "no replica opens to close");
}

#[test]
fn a_titled_commission_is_distinguishable_in_an_issue_list() {
    // #5233: every replica rendered the constant `Bloomery replica — {status}`,
    // so six freshly authored commissions were six indistinguishable rows and
    // the only distinguishing text lived in the body. A heading the issue-title
    // gate accepts is the title; otherwise the floor names the workpiece.
    let projection = GithubProjection::new(FakeGithub::new());
    let mut open = commission("wp-titled", None);
    open.title = "feat(bloomery-github): refuse a contradictory workpiece".to_owned();

    let number = projection.project_commission(&open).expect("create").expect("owns a replica");
    assert_eq!(
        projection.client().issue_title(number).as_deref(),
        Some("feat(bloomery-github): refuse a contradictory workpiece"),
    );

    let untitled = projection
        .project_commission(&commission("wp-untitled", None))
        .expect("create untitled")
        .expect("owns a replica");
    let untitled_title = projection.client().issue_title(untitled).expect("untitled replica");
    assert_eq!(untitled_title, commission_floor_title("wp-untitled"));
    assert_ne!(
        projection.client().issue_title(number).as_deref(),
        Some(untitled_title.as_str()),
        "two commissions remain distinguishable in an issue list",
    );
}

#[test]
fn a_section_heading_falls_back_to_a_title_the_gate_accepts() {
    // Tripwire: #5373, #5374, and #5375 opened as `Description — open` and
    // immediately received `invalid-title` from `.github/workflows/issue-labels.yml`.
    let projection = GithubProjection::new(FakeGithub::new());
    let mut open = commission("wp-5379", None);
    open.title = "Description".to_owned();

    let number = projection.project_commission(&open).expect("create").expect("owns a replica");
    let title = projection.client().issue_title(number).expect("the replica exists");
    assert_eq!(title, commission_floor_title("wp-5379"));
    assert!(issue_title_is_valid(&title), "{title}");
}

#[test]
fn a_lifecycle_change_does_not_rewrite_the_title() {
    let projection = GithubProjection::new(FakeGithub::new());
    let mut open = commission("wp-titled", None);
    open.title = "feat(bloomery-github): refuse a contradictory workpiece".to_owned();

    let number = projection.project_commission(&open).expect("create").expect("owns a replica");
    let created_title = projection.client().issue_title(number).expect("created");

    let landed = CommissionProjection { status: "landed".to_owned(), recorded_issue: Some(number), ..open };
    projection.project_commission(&landed).expect("reconcile");
    assert_eq!(
        projection.client().issue_title(number).as_deref(),
        Some(created_title.as_str()),
        "a lifecycle change does not rewrite the title",
    );
}

#[test]
fn a_commission_that_names_an_issue_comments_on_it_and_opens_nothing() {
    // Tripwire: naming the bug — one work item, two rows on the board.
    let fake = FakeGithub::new();
    fake.seed_issue(42, "human");
    let projection = GithubProjection::new(fake);

    projection.project_commission(&commission("issue-42", None)).expect("first");
    projection.project_commission(&commission("issue-42", None)).expect("second");

    assert_eq!(projection.client().issue_count(), 1, "a named object opens no replica");
    assert_eq!(
        projection.client().comments_on(42).len(),
        1,
        "the second projection updates the comment rather than appending"
    );
}

#[test]
fn a_stray_replica_is_retired_onto_its_source_issue() {
    let fake = FakeGithub::new();
    fake.seed_issue(42, "human");
    let projection = GithubProjection::new(fake);
    let replica = CommissionProjectionApi::create_issue(
        projection.client(),
        &NewIssue {
            title: "Bloomery replica — open".into(),
            body: format!(
                "old replica\n\n{}",
                render_marker(&Marker { key: "commission:issue-42".into(), digest: digest(9) })
            ),
        },
    )
    .expect("seed stray replica");

    projection.project_commission(&commission("issue-42", Some(replica.number))).expect("retire onto the source");

    assert_eq!(projection.client().issue_is_closed(replica.number), Some(true), "the stray replica closes");
    let replica_comments = projection.client().comments_on(replica.number);
    assert_eq!(replica_comments.len(), 1, "one retirement comment on the replica");
    assert!(replica_comments[0].contains("#42"), "the replica names its source: {}", replica_comments[0]);
    assert_eq!(projection.client().comments_on(42).len(), 1, "the source carries the commission");

    projection.project_commission(&commission("issue-42", Some(replica.number))).expect("second retire is a no-op");
    assert_eq!(
        projection.client().comments_on(replica.number).len(),
        1,
        "a second projection adds no further retirement comment"
    );
}

#[test]
fn a_commission_with_no_github_home_still_gets_its_replica() {
    let projection = GithubProjection::new(FakeGithub::new());
    let created = projection.project_commission(&commission("wp-1", None)).expect("create");

    assert_eq!(created, Some(projection.client().issue_numbers()[0]));
    assert_eq!(projection.client().issue_count(), 1);
    assert_eq!(projection.client().created_issue_count(), 1);
}
