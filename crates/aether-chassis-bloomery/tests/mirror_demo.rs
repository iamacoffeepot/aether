#![cfg(feature = "github")]

//! The ADR-0149 outward-mirror demo (#3459 step 7 coverage, narrowed by #4663).
//!
//! Drives a synthetic bloom through the real journal (`SqliteStore`) and the
//! pure reducer, assembles the view document, and projects it through the host
//! projection cap shell onto a fake GitHub — the "Done" the issue names:
//!
//! - the mirror appears on the objects the repository already holds (one
//!   comment per member on the issue that member addresses), and the object
//!   count never moves;
//! - deleting a projection and re-projecting rebuilds it from the journal
//!   alone (delete → reappear).
//!
//! No token, no network: the shell is mounted over the adapter's in-process
//! `FakeGithub`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{
    BloomDraft, ConfigRegistry, Decisions, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership,
    ResolvedConfigs, Snapshot, SpendWindow, WorkpieceId, reduce, view_of,
};
use aether_bloomery_github::{GithubProjection, testing::FakeGithub};
use aether_chassis_bloomery::bloomery::ProjectionShell;
use aether_chassis_bloomery::store::{JournalWrite, SqliteStore, StoreBackend};
use aether_data::wire::{from_bytes, to_vec};

/// The two issues the synthetic bloom's members address — objects the
/// repository already holds. The projection opens none of its own.
const MEMBER_ISSUES: [u64; 2] = [4628, 4629];

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn membership(name: &str, revision: u8) -> Membership {
    // The approval binds the member's subject (ADR-0174), which is only
    // computable once the rest of the member is built.
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
/// journal-then-reduce path, not a hand-built snapshot.
fn synthetic_bloom_snapshot() -> Snapshot {
    let base = digest(0);
    let spec = BloomDraft {
        proposals: vec![
            membership(&format!("issue-{}", MEMBER_ISSUES[0]), 10),
            membership(&format!("issue-{}", MEMBER_ISSUES[1]), 20),
        ],
        base,
        ..BloomDraft::default()
    }
    .seal();

    let event = Event { idempotency_key: IdempotencyKey("seal-1".into()), fact: Fact::Seal(spec) };
    let bytes = to_vec(&event).unwrap();

    // Decide once at admission and journal the decision beside the event
    // (ADR-0190) — the shape every production journal row has.
    let decided = reduce(&Snapshot::new(base), &event, &ResolvedConfigs::default(), &SpendWindow::default());
    let mut store = SqliteStore::open(":memory:").unwrap();
    store
        .append_event(&JournalWrite {
            idempotency_key: "seal-1",
            event: &bytes,
            decisions: &to_vec(&decided).unwrap(),
            decider: "mirror-demo",
        })
        .unwrap();

    // Rebuild the snapshot purely from the journal — fold each record's
    // recorded decision, exactly as recovery replay does.
    let mut snapshot = Snapshot::new(base);
    for record in store.replay_journal().unwrap() {
        let replayed: Event = from_bytes(&record.event).unwrap();
        let decisions: Decisions = from_bytes(&record.decisions).unwrap();
        snapshot = snapshot.apply(&replayed, &decisions, &ResolvedConfigs::default());
    }
    snapshot
}

#[test]
fn a_synthetic_bloom_projects_onto_existing_objects_and_rebuilds_after_deletion() {
    let snapshot = synthetic_bloom_snapshot();
    let view = view_of(&snapshot, |_| None);

    // Mount the projection cap shell over a fake GitHub already holding the two
    // member issues; keep a handle to introspect what the shell projects.
    let fake = FakeGithub::new();
    for number in MEMBER_ISSUES {
        fake.seed_issue(number, "a person wrote this issue");
    }
    let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

    // The mirror: one folded comment per member, on the issue that member
    // addresses. Nothing is opened.
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.issue_count(), MEMBER_ISSUES.len(), "the mirror opens no object of its own");
    for number in MEMBER_ISSUES {
        assert_eq!(fake.comments_on(number).len(), 1, "one folded comment per member");
    }

    // Idempotent: a second reconcile of the same view writes nothing new.
    let after_first = fake.comment_ids_on(MEMBER_ISSUES[0]);
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.comment_count(), MEMBER_ISSUES.len(), "reconcile is idempotent");
    assert_eq!(fake.comment_ids_on(MEMBER_ISSUES[0]), after_first);

    // Delete → reappear: an operator deletes a projection; re-projecting from
    // the same journal-derived view rebuilds it.
    let victim = after_first[0];
    fake.delete_comment(victim);
    assert!(fake.comments_on(MEMBER_ISSUES[0]).is_empty());

    shell.reconcile_view(&view).unwrap();
    let rebuilt = fake.comment_ids_on(MEMBER_ISSUES[0]);
    assert_eq!(rebuilt.len(), 1, "the deleted projection was rebuilt from the journal");
    assert_ne!(rebuilt[0], victim, "the rebuilt comment is a fresh object");
    assert_eq!(fake.issue_count(), MEMBER_ISSUES.len(), "the rebuild opened nothing either");
}
