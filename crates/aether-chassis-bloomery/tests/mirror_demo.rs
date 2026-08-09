//! The ADR-0149 outward-mirror demo (#3459 step 7 coverage).
//!
//! Drives a synthetic bloom through the real journal (`SqliteStore`) and the
//! pure reducer, assembles the view document, and projects it through the host
//! projection cap shell onto a fake GitHub — the "Done" the issue names:
//!
//! - comments appear on the existing source issues and landing PR, no shadow
//!   issue is opened;
//! - re-projecting is idempotent.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{
    BloomDraft, ConfigRegistry, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership,
    ResolvedConfigs, Snapshot, WorkpieceId, reduce, view_of,
};
use aether_bloomery_github::{GithubProjection, NewPullRequest, PullRequestApi, short_hex, testing::FakeGithub};
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
fn a_synthetic_bloom_projects_comments_on_existing_issues_and_pr() {
    let snapshot = synthetic_bloom_snapshot();
    let view = view_of(&snapshot, |_| None);
    let bloom_id = view.blooms[0].id;

    let fake = FakeGithub::new();
    // Source issues already exist where a person already looks.
    fake.seed_issue(1, "issue 1");
    fake.seed_issue(2, "issue 2");
    // Landing PR is the aggregate view.
    let branch = format!("bloom/{}/landing", short_hex(&bloom_id.0));
    let sha = fake.seed_commit("tree");
    fake.seed_ref(&format!("heads/{branch}"), &sha);
    fake.create_pull_request(&NewPullRequest {
        title: format!("land {}", short_hex(&bloom_id.0)),
        body: String::new(),
        head: branch,
        base: "main".into(),
    })
    .unwrap();

    let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

    let before_issues = fake.issue_count();
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.issue_count(), before_issues, "no new issue opened — projection lands on existing objects");
    assert!(fake.comment_count() >= 3, "approvals on source issues + bloom aggregate on PR");

    // Idempotent
    let after = fake.comment_count();
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.comment_count(), after, "reconcile is idempotent");

    // Verify no repository-wide scan was used
    assert_eq!(fake.find_issue_calls(), 0, "no list-and-scan");
}
