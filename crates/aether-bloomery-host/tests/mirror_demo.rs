//! The ADR-0149 outward-mirror demo (#3459 step 7 coverage).
//!
//! Drives a synthetic bloom through the real journal (`SqliteStore`) and the
//! pure reducer, assembles the view document, and projects it through the host
//! projection cap shell onto a fake GitHub — the "Done" the issue names:
//!
//! - a carbon copy appears (an umbrella issue plus a workpiece issue per
//!   member);
//! - deleting a projection and re-projecting rebuilds it from the journal
//!   alone (delete → reappear).
//!
//! No token, no network: the shell is mounted over the adapter's in-process
//! `FakeGithub`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{
    BloomDraft, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, Membership, Snapshot, StageCatalog,
    WorkpieceId, reduce, view_of,
};
use aether_bloomery_github::{GithubProjection, testing::FakeGithub};
use aether_bloomery_host::bloomery::ProjectionShell;
use aether_bloomery_host::store::{SqliteStore, StoreBackend};
use aether_data::wire::{from_bytes, to_vec};

fn digest(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

fn membership(name: &str, revision: u8) -> Membership {
    let scope_revision = digest(revision);
    Membership {
        workpiece: WorkpieceId(name.into()),
        scope_revision,
        approval: Evidence { subject: scope_revision, kind: EvidenceKind::Approval, detail: digest(200) },
    }
}

/// Seal a two-member synthetic bloom, journal the seal event, then replay the
/// journal through the reducer to rebuild the snapshot — the real
/// journal-then-reduce path, not a hand-built snapshot.
fn synthetic_bloom_snapshot() -> Snapshot {
    let base = digest(0);
    let spec = BloomDraft {
        proposals: vec![membership("reactor-core", 10), membership("coolant-loop", 20)],
        base,
        stage_catalog: StageCatalog::line_digest(),
        ..BloomDraft::default()
    }
    .seal();

    let event = Event { idempotency_key: IdempotencyKey("seal-1".into()), fact: Fact::Seal(spec) };
    let bytes = to_vec(&event).unwrap();

    let mut store = SqliteStore::open(":memory:").unwrap();
    store.append_event("seal-1", &bytes).unwrap();

    // Rebuild the snapshot purely from the journal — reduce then apply, event
    // by event, exactly as recovery replay does.
    let mut snapshot = Snapshot::new(base);
    for record in store.replay_journal().unwrap() {
        let replayed: Event = from_bytes(&record.event).unwrap();
        snapshot = snapshot.apply(&replayed, &reduce(&snapshot, &replayed));
    }
    snapshot
}

#[test]
fn a_synthetic_bloom_projects_a_carbon_copy_and_rebuilds_after_deletion() {
    let snapshot = synthetic_bloom_snapshot();
    let view = view_of(&snapshot, |_| None);

    // Mount the projection cap shell over a fake GitHub; keep a handle to
    // introspect what the shell projects.
    let fake = FakeGithub::new();
    let shell = ProjectionShell::new(Arc::new(GithubProjection::new(fake.clone())));

    // Carbon copy: 1 umbrella issue + 2 workpiece issues.
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.issue_count(), 3, "an umbrella issue plus one issue per workpiece");
    let full = fake.issue_numbers();

    // Idempotent: a second reconcile of the same view creates nothing new.
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.issue_numbers(), full, "reconcile is idempotent");

    // Delete → reappear: an operator deletes a projection; re-projecting from
    // the same journal-derived view rebuilds it.
    let victim = full[1];
    fake.delete_issue(victim);
    assert_eq!(fake.issue_count(), 2);
    shell.reconcile_view(&view).unwrap();
    assert_eq!(fake.issue_count(), 3, "the deleted projection was rebuilt from the journal");
    assert!(!fake.issue_numbers().contains(&victim), "the rebuilt issue is a fresh object");
}
