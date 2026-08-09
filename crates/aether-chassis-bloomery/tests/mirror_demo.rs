//! The ADR-0149 outward-mirror demo: projection onto existing objects.
//!
//! Drives a synthetic bloom through the real journal and projects it onto
//! source issues and its landing PR — no shadow issues are opened.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{
    BloomDraft, BloomId, ConfigRegistry, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership,
    ResolvedConfigs, Snapshot, WorkpieceId, reduce, view_of,
};
use aether_bloomery_github::client::{NewPullRequest, PullRequestApi};
use aether_bloomery_github::{GithubProjection, short_hex, testing::FakeGithub};
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
        proposals: vec![membership("issue-11", 10), membership("issue-22", 20)],
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
fn a_synthetic_bloom_projects_onto_source_issues_and_pr() {
    let snapshot = synthetic_bloom_snapshot();
    let view = view_of(&snapshot, |_| None);
    // The bloom id is the sealed spec's digest; its landing PR branch is derived.
    let bloom_id = view.blooms[0].id;
    let branch = format!("bloom/{}/landing", short_hex(&bloom_id.0));

    let fake = FakeGithub::new();
    // The bloom aggregate needs its landing PR to have a home; seed it.
    let sha = fake.seed_commit("tree");
    fake.seed_ref(&format!("heads/{branch}"), &sha);
    fake.create_pull_request(&NewPullRequest {
        title: "landing".into(),
        body: "body".into(),
        head: branch,
        base: "main".into(),
    })
    .unwrap();

    let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

    shell.reconcile_view(&view).unwrap();
    // No shadow issues.
    assert_eq!(fake.issue_count(), 0, "no shadow issues are opened");
    // Bloom aggregate on PR (1) + 2 members each with member-view + approval (4) =5
    assert_eq!(fake.comment_count(), 5, "projection is comments on source issues and PR");

    // Idempotent: second reconcile creates nothing new.
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.comment_count(), 5, "reconcile is idempotent");
    assert_eq!(fake.issue_count(), 0);

    // Rebuild via comment idempotency: a second reconcile after no mutation stays converged.
    // (Issue delete → reappear is retired; comments rebuild via marker.)
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.comment_count(), 5);
}
