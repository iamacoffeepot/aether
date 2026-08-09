//! The ADR-0149 outward-mirror demo — projects onto existing source issues
//! and the landing pull request, never opening shadow issues.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{
    BloomDraft, ConfigRegistry, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership,
    ResolvedConfigs, Snapshot, WorkpieceId, reduce, view_of,
};
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

fn synthetic_bloom_snapshot() -> Snapshot {
    let base = digest(0);
    let spec = BloomDraft {
        proposals: vec![membership("issue-101", 10), membership("issue-102", 20)],
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
fn a_synthetic_bloom_projects_comments_to_source_issues_and_is_idempotent() {
    let snapshot = synthetic_bloom_snapshot();
    let view = view_of(&snapshot, |_| None);

    let fake = FakeGithub::new();
    let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.issue_count(), 0, "no shadow issue is opened — source issues already exist");
    let comments_first = fake.comment_count();
    assert!(comments_first >= 2, "member evidence projects as comments on source issues");

    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.comment_count(), comments_first, "reconcile is idempotent");
}
