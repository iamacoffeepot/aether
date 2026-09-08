//! The drain → seal core of the propose reactor, over a real `SqliteStore`,
//! `SqliteCorrespondence`, and a recording pusher — without the mail harness.
//!
//! Pins the durability the async admit used to skip: a seal is persisted on
//! the outbox row before the prefix advances, a recorded receipt redrives the
//! same Event without republishing, and the journal is the acknowledgement
//! oracle. Publication before the receipt row remains a window; these tests
//! do not claim exactly-once for that host effect.

use std::path::{Path, PathBuf};
use std::slice::from_ref;
use std::sync::Mutex;

use aether_bloomery::testing::digest;
use aether_bloomery::{
    Admit, BackendObjectId, CandidateRef, Correspondence, Digest, Event, OperatorProposal, ProposalPayload, Topic,
    WorkpieceId, encode_hex,
};
use aether_bloomery_github::candidate_ref_name;
use aether_data::wire::{from_bytes, to_vec};

use super::{drain_and_seal, proposal_spec, seal_event};
use crate::bloomery::CandidatePush;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::{JournalWrite, SqliteCorrespondence, SqliteStore, StoreBackend};

#[derive(Default)]
struct RecordingPush {
    pushed: Mutex<Vec<(String, String)>>,
}

impl CandidatePush for RecordingPush {
    fn push(&self, commit_hex: &str, target_ref: &str) -> Result<(), String> {
        self.pushed
            .lock()
            .expect("the recording pusher is not poisoned")
            .push((commit_hex.to_owned(), target_ref.to_owned()));
        Ok(())
    }
}

impl RecordingPush {
    fn pairs(&self) -> Vec<(String, String)> {
        self.pushed.lock().expect("the recording pusher is not poisoned").clone()
    }
}

struct FailingPush;

impl CandidatePush for FailingPush {
    fn push(&self, _commit_hex: &str, _target_ref: &str) -> Result<(), String> {
        Err("origin refused the candidate".to_owned())
    }
}

fn proposal(reason: &str, tree: u8, checkout: u8) -> OperatorProposal {
    OperatorProposal {
        candidate: CandidateRef { tree: digest(tree), checkout: digest(checkout) },
        reason: reason.to_owned(),
        operator: "tester".to_owned(),
    }
}

fn object(seed: u8) -> BackendObjectId {
    BackendObjectId::new(vec![seed; 20])
}

fn bind(correspondence: &SqliteCorrespondence, checkout: Digest, object: &BackendObjectId) {
    correspondence.record(&checkout, object).expect("the checkout correspondence records");
}

fn empty_correspondence() -> SqliteCorrespondence {
    SqliteCorrespondence::open(":memory:").expect("an in-memory correspondence store opens")
}

fn enqueue_proposal(store: &mut SqliteStore, proposal: &OperatorProposal, base: Digest) -> u64 {
    let payload = ProposalPayload { proposal: proposal.clone(), base };
    store
        .enqueue_topic(Topic::Proposal, &to_vec(&payload).expect("the payload encodes"), None)
        .expect("the enqueue lands")
}

fn admitted_event(admit: &Admit) -> Event {
    from_bytes(&admit.event).expect("the admit carries an event")
}

fn journal_event(store: &mut SqliteStore, event: &Event) {
    let bytes = to_vec(event).expect("the event encodes");
    store
        .append_event(&JournalWrite {
            idempotency_key: &event.idempotency_key.0,
            event: &bytes,
            decisions: b"decided",
            decider: "test",
        })
        .expect("the journal append lands");
}

fn file_store() -> (tempfile::TempDir, PathBuf, SqliteStore) {
    let dir = tempfile::tempdir().expect("a temp dir binds");
    let path = dir.path().join("store.sqlite");
    let store = SqliteStore::open(path.to_str().expect("utf-8 path")).expect("a file store opens");
    (dir, path, store)
}

fn reopen_store(path: &Path) -> SqliteStore {
    SqliteStore::open(path.to_str().expect("utf-8 path")).expect("the same file reopens")
}

fn assert_unacked(store: &mut SqliteStore, sequence: u64, ack_through: Option<u64>) {
    assert_eq!(ack_through, None, "receipt persistence is not acknowledgement; the journal is");
    assert_eq!(
        store.drain_topic(Topic::Proposal).expect("the drain reads").first().map(|entry| entry.sequence),
        Some(sequence),
        "the entry stays undelivered until the journal holds the seal key",
    );
}

fn assert_no_receipt(store: &mut SqliteStore, sequence: u64) {
    assert!(
        store.outbox_results(Topic::Proposal.as_str(), sequence).expect("the result lookup succeeds").is_none(),
        "a failed proposal must not record a receipt",
    );
}

fn composition_target(proposal: &OperatorProposal, base: Digest) -> String {
    candidate_ref_name(&proposal_spec(proposal, base).id(), WorkpieceId::COMPOSITION)
}

#[test]
fn a_proposal_seals_without_acking_until_the_journal_confirms() {
    // Tripwire: acking at Admit used to drop a seal whose detached mail never
    // reached the journal. The first completion must retain the row and the
    // exact Event.
    let proposal = proposal("first", 1, 2);
    let base = digest(10);
    let object = object(0xcc);
    let correspondence = empty_correspondence();
    bind(&correspondence, proposal.candidate.checkout, &object);
    let pusher = RecordingPush::default();
    let (_dir, path, mut store) = file_store();
    let sequence = enqueue_proposal(&mut store, &proposal, base);

    let (admits, ack_through) = drain_and_seal(&mut store, &correspondence, &pusher, true).expect("the drain succeeds");

    assert_eq!(admits.len(), 1, "one proposal admits one seal");
    assert_unacked(&mut store, sequence, ack_through);
    assert_eq!(admitted_event(&admits[0]), seal_event(&proposal, base));
    assert_eq!(
        store.outbox_results(Topic::Proposal.as_str(), sequence).expect("the result lookup succeeds").as_deref(),
        Some(from_ref(&seal_event(&proposal, base))),
        "the sidecar keeps the exact Seal Event",
    );
    assert_eq!(
        pusher.pairs().as_slice(),
        &[(encode_hex(object.as_bytes()), composition_target(&proposal, base))],
        "the candidate publishes onto the composition ref for this bloom",
    );
    drop(store);

    let mut store = reopen_store(&path);
    let (again, ack_through) =
        drain_and_seal(&mut store, &empty_correspondence(), &pusher, true).expect("the redrive succeeds");
    assert_eq!(ack_through, None, "a pending receipt does not ack");
    assert_eq!(again, admits, "the exact Seal bytes are retained");
    assert_eq!(pusher.pairs().len(), 1, "a recorded receipt does not publish again");
}

#[test]
fn a_journaled_seal_acks_without_publication_or_correspondence() {
    // Tripwire: once control has journaled the seal, restart recovers the ack
    // from the sidecar and must not re-read correspondence or push again.
    let proposal = proposal("journaled", 1, 2);
    let base = digest(10);
    let object = object(0xcc);
    let correspondence = empty_correspondence();
    bind(&correspondence, proposal.candidate.checkout, &object);
    let pusher = RecordingPush::default();
    let (_dir, path, mut store) = file_store();
    let sequence = enqueue_proposal(&mut store, &proposal, base);

    let (admits, ack_through) = drain_and_seal(&mut store, &correspondence, &pusher, true).expect("the drain succeeds");
    assert_eq!(ack_through, None);
    journal_event(&mut store, &admitted_event(&admits[0]));
    drop(store);

    let mut store = reopen_store(&path);
    let (again, ack_through) =
        drain_and_seal(&mut store, &empty_correspondence(), &pusher, true).expect("the redrive succeeds");
    assert!(again.is_empty(), "a journaled receipt does not resend");
    assert_eq!(ack_through, Some(sequence), "journal confirmation acknowledges without a host redrive");
    assert_eq!(pusher.pairs().len(), 1, "a journaled receipt does not publish again");
}

#[test]
fn a_pending_prior_proposal_blocks_a_later_publication() {
    // Tripwire: a persisted-but-unjournaled prefix row must not let a later
    // proposal publish. The later candidate would otherwise land while the
    // earlier seal is still awaiting control.
    let first = proposal("first", 1, 2);
    let second = proposal("second", 3, 4);
    let base = digest(10);
    let (first_object, second_object) = (object(0xcc), object(0xdd));
    let correspondence = empty_correspondence();
    bind(&correspondence, first.candidate.checkout, &first_object);
    bind(&correspondence, second.candidate.checkout, &second_object);
    let pusher = RecordingPush::default();
    let (_dir, _path, mut store) = file_store();
    let first_sequence = enqueue_proposal(&mut store, &first, base);
    let _second_sequence = enqueue_proposal(&mut store, &second, base);

    let (admits, ack_through) = drain_and_seal(&mut store, &correspondence, &pusher, true).expect("the drain succeeds");
    assert_unacked(&mut store, first_sequence, ack_through);
    assert_eq!(admitted_event(&admits[0]), seal_event(&first, base));
    assert_eq!(pusher.pairs().len(), 1, "the later proposal does not publish behind a pending prefix");

    let (again, ack_through) =
        drain_and_seal(&mut store, &correspondence, &pusher, true).expect("the redrive succeeds");
    assert_eq!(ack_through, None);
    assert_eq!(again, admits, "the pending prefix still holds later rows");
    assert_eq!(pusher.pairs().len(), 1, "redrive of a pending prefix does not publish the later proposal");
}

#[test]
fn a_journaled_prefix_admits_the_next_proposal_without_overacking() {
    // Tripwire: journal confirmation of the prefix must release the next
    // distinct seal without acknowledging it in the same pass.
    let first = proposal("first", 1, 2);
    let second = proposal("second", 3, 4);
    let base = digest(10);
    let (first_object, second_object) = (object(0xcc), object(0xdd));
    let correspondence = empty_correspondence();
    bind(&correspondence, first.candidate.checkout, &first_object);
    bind(&correspondence, second.candidate.checkout, &second_object);
    let pusher = RecordingPush::default();
    let (_dir, path, mut store) = file_store();
    let first_sequence = enqueue_proposal(&mut store, &first, base);
    let second_sequence = enqueue_proposal(&mut store, &second, base);

    let (admits, ack_through) = drain_and_seal(&mut store, &correspondence, &pusher, true).expect("the drain succeeds");
    assert_eq!(ack_through, None);
    journal_event(&mut store, &admitted_event(&admits[0]));
    drop(store);

    let mut store = reopen_store(&path);
    let (again, ack_through) =
        drain_and_seal(&mut store, &correspondence, &pusher, true).expect("the follow-up drain succeeds");
    assert_eq!(ack_through, Some(first_sequence), "only the journaled prefix is acknowledged");
    assert_eq!(again.len(), 1, "the next unrecorded proposal seals once the prefix is journaled");
    assert_eq!(admitted_event(&again[0]), seal_event(&second, base));
    store.ack_topic(Topic::Proposal, first_sequence).expect("the journaled prefix acks");
    assert_eq!(
        store.drain_topic(Topic::Proposal).expect("the drain reads").first().map(|entry| entry.sequence),
        Some(second_sequence),
        "the later receipt is not acknowledged with the prefix",
    );
    assert_eq!(
        pusher.pairs().as_slice(),
        &[
            (encode_hex(first_object.as_bytes()), composition_target(&first, base)),
            (encode_hex(second_object.as_bytes()), composition_target(&second, base)),
        ],
        "each proposal publishes onto its own composition ref",
    );
}

#[test]
fn publish_disabled_still_persists_the_seal_receipt() {
    // Tripwire: fixture boots skip the candidate push; the receipt protocol
    // must still persist the Seal and wait for the journal.
    let proposal = proposal("unpublished", 1, 2);
    let base = digest(10);
    let correspondence = empty_correspondence();
    bind(&correspondence, proposal.candidate.checkout, &object(0xcc));
    let pusher = RecordingPush::default();
    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let sequence = enqueue_proposal(&mut store, &proposal, base);

    let (admits, ack_through) =
        drain_and_seal(&mut store, &correspondence, &pusher, false).expect("the drain succeeds");

    assert_eq!(admits.len(), 1);
    assert_unacked(&mut store, sequence, ack_through);
    assert_eq!(admitted_event(&admits[0]), seal_event(&proposal, base));
    assert!(pusher.pairs().is_empty(), "publish_candidate=false must not push");

    journal_event(&mut store, &admitted_event(&admits[0]));
    let (again, ack_through) =
        drain_and_seal(&mut store, &empty_correspondence(), &pusher, false).expect("the redrive succeeds");
    assert!(again.is_empty());
    assert_eq!(ack_through, Some(sequence));
    assert!(pusher.pairs().is_empty());
}

#[test]
fn missing_correspondence_leaves_the_row_unrecorded_and_retries() {
    let proposal = proposal("missing", 1, 2);
    let base = digest(10);
    let object = object(0xcc);
    let pusher = RecordingPush::default();
    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let sequence = enqueue_proposal(&mut store, &proposal, base);

    let (admits, ack_through) =
        drain_and_seal(&mut store, &empty_correspondence(), &pusher, true).expect("the drain succeeds");
    assert!(admits.is_empty(), "a missing checkout cannot seal");
    assert_unacked(&mut store, sequence, ack_through);
    assert_no_receipt(&mut store, sequence);
    assert!(pusher.pairs().is_empty(), "a missing checkout must not publish");

    let correspondence = empty_correspondence();
    bind(&correspondence, proposal.candidate.checkout, &object);
    let (admits, ack_through) = drain_and_seal(&mut store, &correspondence, &pusher, true).expect("the retry succeeds");
    assert_eq!(admits.len(), 1);
    assert_unacked(&mut store, sequence, ack_through);
    assert_eq!(admitted_event(&admits[0]), seal_event(&proposal, base));
    assert_eq!(pusher.pairs().as_slice(), &[(encode_hex(object.as_bytes()), composition_target(&proposal, base))],);
}

#[test]
fn a_push_failure_leaves_the_row_unrecorded_and_retries() {
    let proposal = proposal("refused", 1, 2);
    let base = digest(10);
    let object = object(0xcc);
    let correspondence = empty_correspondence();
    bind(&correspondence, proposal.candidate.checkout, &object);
    let mut store = SqliteStore::open(":memory:").expect("an in-memory store opens");
    let sequence = enqueue_proposal(&mut store, &proposal, base);

    let (admits, ack_through) =
        drain_and_seal(&mut store, &correspondence, &FailingPush, true).expect("the drain succeeds");
    assert!(admits.is_empty(), "a refused push cannot seal");
    assert_unacked(&mut store, sequence, ack_through);
    assert_no_receipt(&mut store, sequence);

    let pusher = RecordingPush::default();
    let (admits, ack_through) = drain_and_seal(&mut store, &correspondence, &pusher, true).expect("the retry succeeds");
    assert_eq!(admits.len(), 1);
    assert_unacked(&mut store, sequence, ack_through);
    assert_eq!(admitted_event(&admits[0]), seal_event(&proposal, base));
    assert_eq!(pusher.pairs().as_slice(), &[(encode_hex(object.as_bytes()), composition_target(&proposal, base))],);
}
