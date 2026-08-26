//! Archive-pass coverage: eligibility, the between-blooms refusal, collision
//! disambiguation, and a move that cannot complete.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use aether_bloomery::{KeyId, Observation, Provenance, SignatureEnvelope, Statement, WorkpieceId};

use super::super::tests::{Released, StubRunner, age_dir, journal_landed, order_for, policy};
use super::super::{ArchiveRequest, ArchiveTier, archive_pass};
use super::ArchiveOutcome;
use crate::store::{CommissionBackend, SqliteStore, StoreBackend};

fn intent(id: &str) -> Statement {
    Statement {
        words: format!("intent-{id}").into_bytes(),
        provenance: Provenance::ObservationAttestation(Observation { source: "test".to_owned() }),
        parents: Vec::new(),
    }
}

fn plant_commission(store: &mut SqliteStore, id: &str, cancelled: bool) {
    let digest = store.create(&WorkpieceId(id.to_owned()), &intent(id)).expect("the commission is created");
    if cancelled {
        store
            .cancel(
                &WorkpieceId(id.to_owned()),
                &Statement {
                    words: digest.as_bytes().to_vec(),
                    provenance: Provenance::AuthorSignature(SignatureEnvelope {
                        signer: KeyId("op".to_owned()),
                        signature: vec![1],
                    }),
                    parents: Vec::new(),
                },
            )
            .expect("the commission cancels");
    }
}

fn runner(registered: Vec<PathBuf>) -> StubRunner {
    StubRunner { registered, released: Released(Arc::new(Mutex::new(Vec::new()))) }
}

fn pass(
    store: &mut SqliteStore,
    runner: &StubRunner,
    worktree_base: &Path,
    tier: &ArchiveTier,
    now: SystemTime,
    retention_days: u64,
) -> ArchiveOutcome {
    archive_pass(&mut ArchiveRequest {
        store,
        runner,
        worktree_base,
        tier,
        policy: &policy(retention_days, u64::MAX),
        now,
    })
    .expect("the archive pass reads the in-memory store")
}

#[test]
fn aged_evidence_of_a_landed_bloom_lands_on_the_tier_intact() {
    // The plausible bug: the pass reports success while leaving the working
    // copy, or moves the directory but drops its files.
    let scratch = tempfile::tempdir().expect("a working root is created");
    let archive = tempfile::tempdir().expect("an archive root is created");
    let nonce = "stale-evidence";
    let evidence = scratch.path().join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence).expect("the evidence dir is created");
    fs::write(evidence.join("transcript.jsonl"), "kept\n").expect("the transcript writes");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let bloom = journal_landed(&mut store);
    store.record_order(&order_for(nonce, &bloom)).expect("the order is recorded");
    store.consume_order(nonce).expect("the order is consumed");

    let now = SystemTime::now();
    age_dir(&evidence, now, 8);

    let runner = runner(Vec::new());
    let tier = ArchiveTier::new(archive.path().to_path_buf());
    let outcome = pass(&mut store, &runner, scratch.path(), &tier, now, 7);

    let ArchiveOutcome::Archived { records, failures } = outcome else {
        panic!("a between-blooms host archives: {outcome:?}");
    };
    assert!(failures.is_empty(), "the move completes: {failures:?}");
    assert_eq!(records.len(), 1);
    assert!(!evidence.exists(), "the working copy is gone");
    let dest = archive.path().join("evidence").join(format!("{nonce}-evidence"));
    assert!(dest.is_dir(), "the record is on the tier");
    assert_eq!(fs::read(dest.join("transcript.jsonl")).expect("the archived file reads"), b"kept\n");
}

#[test]
fn an_outstanding_order_refuses_the_whole_pass_and_moves_nothing() {
    // ADR-0211: a pass that archives while work walks is the 2026-08-25
    // failure under a new name. The refusal must name the walking order and
    // leave every candidate where it was.
    let scratch = tempfile::tempdir().expect("a working root is created");
    let archive = tempfile::tempdir().expect("an archive root is created");
    let nonce = "stale-evidence";
    let evidence = scratch.path().join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence).expect("the evidence dir is created");
    fs::write(evidence.join("kept"), b"bytes").expect("the marker writes");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let bloom = journal_landed(&mut store);
    store.record_order(&order_for(nonce, &bloom)).expect("the order is recorded");

    let now = SystemTime::now();
    age_dir(&evidence, now, 8);

    let runner = runner(Vec::new());
    let tier = ArchiveTier::new(archive.path().to_path_buf());
    let outcome = pass(&mut store, &runner, scratch.path(), &tier, now, 7);

    let ArchiveOutcome::Refused { reason } = outcome else {
        panic!("an outstanding order must refuse: {outcome:?}");
    };
    assert!(reason.contains(nonce), "the refusal names the walking order: {reason}");
    assert!(evidence.is_dir(), "the candidate stays in the working root");
    assert!(tier.list().expect("the empty tier lists").is_empty());
}

#[test]
fn a_session_tree_archives_only_when_its_commission_has_resolved() {
    // "No live member names this slug" is not enough: an open commission's
    // tree is still the conversation. Unknown / open reads as live.
    let scratch = tempfile::tempdir().expect("a working root is created");
    let archive = tempfile::tempdir().expect("an archive root is created");
    let open = scratch.path().join("sessions").join("s-open").join("tree");
    let cancelled = scratch.path().join("sessions").join("s-cancelled").join("tree");
    fs::create_dir_all(&open).expect("the open session's tree is created");
    fs::create_dir_all(&cancelled).expect("the cancelled session's tree is created");
    fs::write(cancelled.join("note"), b"figured-out").expect("the record writes");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let bloom = journal_landed(&mut store);
    store.record_session_slug(bloom.0.as_bytes(), "wp-open", "s-open").expect("the open slug records");
    store.record_session_slug(bloom.0.as_bytes(), "wp-cancelled", "s-cancelled").expect("the cancelled slug records");
    plant_commission(&mut store, "wp-open", false);
    plant_commission(&mut store, "wp-cancelled", true);

    let runner = runner(vec![open.clone(), cancelled.clone()]);
    let tier = ArchiveTier::new(archive.path().to_path_buf());
    let outcome = pass(&mut store, &runner, scratch.path(), &tier, SystemTime::now(), 7);

    let ArchiveOutcome::Archived { records, failures } = outcome else {
        panic!("a between-blooms host archives: {outcome:?}");
    };
    assert!(failures.is_empty(), "the cancelled tree moves: {failures:?}");
    assert_eq!(records.len(), 1, "only the resolved commission's tree moves");
    assert_eq!(records[0].name, "s-cancelled");
    assert!(open.is_dir(), "an open commission's tree stays");
    assert!(!cancelled.exists(), "the cancelled tree left the working root");
    assert_eq!(
        fs::read(archive.path().join("sessions").join("s-cancelled").join("note")).expect("the archived note reads"),
        b"figured-out"
    );
}

#[test]
fn a_name_already_on_the_tier_is_disambiguated_rather_than_overwritten() {
    // Two records of the same name must both survive. Overwriting the one
    // already on the tier is the one outcome worse than leaving the new one.
    let scratch = tempfile::tempdir().expect("a working root is created");
    let archive = tempfile::tempdir().expect("an archive root is created");
    let existing = archive.path().join("evidence").join("same-evidence");
    fs::create_dir_all(&existing).expect("the existing archived dir is created");
    fs::write(existing.join("old"), b"first").expect("the first copy writes");

    let incoming = scratch.path().join("same-evidence");
    fs::create_dir_all(&incoming).expect("the incoming evidence dir is created");
    fs::write(incoming.join("new"), b"second").expect("the second copy writes");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let bloom = journal_landed(&mut store);
    store.record_order(&order_for("same", &bloom)).expect("the order is recorded");
    store.consume_order("same").expect("the order is consumed");
    let now = SystemTime::now();
    age_dir(&incoming, now, 8);

    let runner = runner(Vec::new());
    let tier = ArchiveTier::new(archive.path().to_path_buf());
    let outcome = pass(&mut store, &runner, scratch.path(), &tier, now, 7);
    let ArchiveOutcome::Archived { records, failures } = outcome else {
        panic!("a between-blooms host archives: {outcome:?}");
    };
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(records.len(), 1);
    assert_eq!(fs::read(existing.join("old")).expect("the first copy remains"), b"first");
    assert_ne!(records[0].path, existing, "the incoming copy took a new name");
    assert_eq!(fs::read(records[0].path.join("new")).expect("the second copy is intact"), b"second");
}

#[test]
fn a_move_that_cannot_complete_leaves_the_source_readable() {
    // A half-archived record is the one outcome worse than an un-archived one.
    // Pointing the class directory at a file makes the destination uncreatable.
    let scratch = tempfile::tempdir().expect("a working root is created");
    let archive = tempfile::tempdir().expect("an archive root is created");
    fs::write(archive.path().join("evidence"), b"not a directory").expect("the class path is a file");

    let nonce = "blocked-evidence";
    let evidence = scratch.path().join(format!("{nonce}-evidence"));
    fs::create_dir_all(&evidence).expect("the evidence dir is created");
    fs::write(evidence.join("kept"), b"still here").expect("the marker writes");

    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let bloom = journal_landed(&mut store);
    store.record_order(&order_for(nonce, &bloom)).expect("the order is recorded");
    store.consume_order(nonce).expect("the order is consumed");
    let now = SystemTime::now();
    age_dir(&evidence, now, 8);

    let runner = runner(Vec::new());
    let tier = ArchiveTier::new(archive.path().to_path_buf());
    let outcome = pass(&mut store, &runner, scratch.path(), &tier, now, 7);
    let ArchiveOutcome::Archived { records, failures } = outcome else {
        panic!("a failed move is a per-record failure, not a refusal: {outcome:?}");
    };
    assert!(records.is_empty(), "nothing landed on the tier");
    assert_eq!(failures.len(), 1, "the blocked move is reported");
    assert!(evidence.is_dir(), "the source is still where it was");
    assert_eq!(fs::read(evidence.join("kept")).expect("the source still reads"), b"still here");
}
