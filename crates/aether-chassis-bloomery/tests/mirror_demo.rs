//! The ADR-0149 outward-mirror demo (revised to project onto existing
//! objects rather than shadow issues).
//!
//! Drives a synthetic bloom through the real journal (`SqliteStore`) and the
//! pure reducer, assembles the view document, and projects it through the host
//! projection cap shell onto a fake GitHub — the "Done" the issue names:
//!
//! - member state appears as comments on their source issues (the `issue-<n>`
//!   issue already exists) and the bloom aggregate as a comment on its landing
//!   pull request — no shadow issues are opened;
//! - deleting a projected comment and re-projecting rebuilds it from the
//!   journal alone (delete → reappear), and reconciling twice is a no-op.
//!
//! No token, no network: the shell is mounted over the adapter's in-process
//! `FakeGithub`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{
    BloomDraft, ConfigRegistry, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership,
    ResolvedConfigs, Snapshot, WorkpieceId, reduce, view_of,
};
use aether_bloomery_github::client::{GithubApi as _, NewIssue, NewPullRequest};
use aether_bloomery_github::{GithubProjection, testing::FakeGithub};
use aether_chassis_bloomery::bloomery::ProjectionShell;
use aether_chassis_bloomery::store::{SqliteStore, StoreBackend};
use aether_data::wire::{from_bytes, to_vec};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn membership(name: &str, revision: u8) -> Membership {
    let mut member = Membership {
        workpiece: WorkpieceId(name.into()),
        scope_revision: digest(revision),
        configs: ConfigRegistry::default(),
        approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
    };
    member.approval.subject = member.subject();
    member
}

/// Seal a two-member synthetic bloom, journal the seal event, then replay the
/// journal through the reducer to rebuild the snapshot — the real
/// journal-then-reduce path, not a hand-built snapshot. Members are
/// `issue-1` and `issue-2` so they map to source issues that already exist.
fn synthetic_bloom_snapshot() -> Snapshot {
    let base = digest(0);
    let spec = BloomDraft {
        proposals: vec![membership("issue-1", 10), membership("issue-2", 20)],
        base,
        ..BloomDraft::default()
    }
    .seal();

    let event = Event { idempotency_key: IdempotencyKey("seal-1".into()), fact: Fact::Seal(spec) };
    let bytes = to_vec(&event).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    store.append_event("seal-1", &bytes).unwrap();

    let mut snapshot = Snapshot::new(base);
    for record in store.replay_journal().unwrap() {
        let replayed: Event = from_bytes(&record.event).unwrap();
        snapshot = snapshot.apply(
            &replayed,
            &reduce(&snapshot, &replayed, &ResolvedConfigs::default()),
            &ResolvedConfigs::default(),
        );
    }
    snapshot
}

#[test]
fn a_synthetic_bloom_projects_a_carbon_copy_and_rebuilds_after_deletion() {
    let snapshot = synthetic_bloom_snapshot();
    let view = view_of(&snapshot, |_| None);
    let bloom_id = view.blooms[0].id;

    // Pre-seed the two source issues that the members map to (issue-1 → 1,
    // issue-2 → 2) and the bloom's landing pull request. These are the
    // objects that already exist and already close themselves.
    let fake = FakeGithub::new();
    for i in 1..=2 {
        fake.create_issue(&NewIssue { title: format!("source {i}"), body: format!("issue {i} body") }).unwrap();
    }
    let branch = format!("bloom/{}/landing", aether_bloomery_github::short_hex(&bloom_id.0));
    let commit = fake.create_commit("landing", "tree", &[]).unwrap();
    fake.seed_ref(&format!("heads/{branch}"), &commit.sha);
    fake.create_pull_request(&NewPullRequest {
        title: format!("landing {}", aether_bloomery_github::short_hex(&bloom_id.0)),
        body: String::new(),
        head: branch,
        base: "main".to_owned(),
    })
    .unwrap();

    let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

    // Carbon copy: member state as comments on source issues and bloom
    // aggregate as a comment on the landing pull request — no new issues.
    let issues_before = fake.issue_count();
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.issue_count(), issues_before, "projection onto existing objects must not open a shadow issue");
    let comments_after_first = fake.comment_count();
    assert!(comments_after_first >= 3, "at least a member comment per workpiece plus a bloom comment on the PR");

    // Idempotent: a second reconcile of the same view creates nothing new.
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.comment_count(), comments_after_first, "reconcile is idempotent");
    assert_eq!(fake.issue_count(), issues_before, "second reconcile still no new issue");

    // Delete → reappear: delete a projected member comment and re-project;
    // the same journal-derived view rebuilds it.
    let issue_one_comments: Vec<u64> = {
        // Find any comment on issue 1 (source issue for issue-1)
        // FakeGithub has no public comment-id enumeration, so delete the first
        // comment we can find via its marker scan by recreating the view.
        // Simpler: delete all comments on issue 1 by directly inspecting fake
        // state via a test-only helper — we expose `delete_comment` for this.
        // For now, delete the member comment by known key: find its id, delete.
        let key = format!("wp:issue-1@bloom:{}", aether_bloomery_github::short_hex(&bloom_id.0));
        fake.find_comment(1, &key).unwrap().map(|c| c.id).into_iter().collect()
    };
    assert!(!issue_one_comments.is_empty(), "a member comment exists to delete");
    for id in issue_one_comments {
        fake.delete_comment(id);
    }
    assert_eq!(fake.comment_count(), comments_after_first - 1);
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.comment_count(), comments_after_first, "the deleted comment was rebuilt from the journal");
}
