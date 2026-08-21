//! Store-backed tests for the notification ledger (#5166).
//!
//! Everything here drives [`deliver`] against a real `:memory:` store and a
//! recording sink, because the logic under test *is* the difference between a
//! document's loud set and the ledger — a fake store would be testing the
//! fake.

use std::sync::{Mutex, PoisonError};

use aether_bloomery::testing::digest;
use aether_bloomery::{
    BloomId, BloomStatus, BloomView, MemberView, StageId, VerifyFailureSet, ViewDocument, Wedge, WorkpieceId,
};
use aether_bloomery_github::{WebhookError, WebhookSink};

use super::deliver;
use crate::store::{SqliteStore, StoreBackend};

/// A sink that records what it was asked to post and can be told to refuse.
#[derive(Default)]
struct RecordingSink {
    posted: Mutex<Vec<String>>,
    refusing: Mutex<bool>,
}

impl RecordingSink {
    fn refuse(&self, refusing: bool) {
        *self.refusing.lock().unwrap_or_else(PoisonError::into_inner) = refusing;
    }

    fn posted(&self) -> Vec<String> {
        self.posted.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl WebhookSink for RecordingSink {
    fn post(&self, content: &str) -> Result<(), WebhookError> {
        if *self.refusing.lock().unwrap_or_else(PoisonError::into_inner) {
            return Err(WebhookError::Status { status: 429 });
        }
        self.posted.lock().unwrap_or_else(PoisonError::into_inner).push(content.to_owned());
        Ok(())
    }
}

fn store() -> SqliteStore {
    SqliteStore::open(":memory:").expect("an in-memory store opens")
}

fn wedged_member(workpiece: &str) -> MemberView {
    let wedge = Wedge { stage: StageId::Verify, evidence: digest(9), repeated_verifiers: VerifyFailureSet::default() };

    MemberView { workpiece: WorkpieceId(workpiece.to_owned()), wedge: Some(wedge), ..MemberView::default() }
}

fn bloom(status: BloomStatus, members: Vec<MemberView>) -> ViewDocument {
    ViewDocument {
        blooms: vec![BloomView { id: BloomId(digest(0xab)), status, members, ..BloomView::default() }],
        ..ViewDocument::default()
    }
}

#[test]
fn one_message_per_transition_and_never_a_second() {
    // The acceptance case: sealed → wedged → landed posts exactly three
    // messages. The plausible bug is the one every polling notifier has —
    // re-posting the whole standing loud set on every tick, which turns an
    // alert channel into noise nobody reads within a day.
    let mut store = store();
    let sink = RecordingSink::default();

    let sealed = bloom(
        BloomStatus::Sealed,
        vec![MemberView { workpiece: WorkpieceId("issue-1".to_owned()), ..MemberView::default() }],
    );
    deliver(&mut store, &sink, &sealed, 1).expect("the ledger writes");
    deliver(&mut store, &sink, &sealed, 2).expect("the ledger writes");

    let wedged = bloom(BloomStatus::Sealed, vec![wedged_member("issue-1")]);
    deliver(&mut store, &sink, &wedged, 3).expect("the ledger writes");
    deliver(&mut store, &sink, &wedged, 4).expect("the ledger writes");

    let landed = bloom(BloomStatus::Landed, vec![wedged_member("issue-1")]);
    deliver(&mut store, &sink, &landed, 5).expect("the ledger writes");

    let posted = sink.posted();
    assert_eq!(posted.len(), 3, "one message per transition, not one per poll: {posted:?}");
    assert!(posted[0].starts_with("sealed  bloom abababababab"), "{posted:?}");
    assert!(posted[1].starts_with("wedge  issue-1 in bloom abababababab"), "{posted:?}");
    assert!(posted[2].starts_with("landed  bloom abababababab"), "{posted:?}");
}

#[test]
fn a_failing_endpoint_leaves_the_message_owed() {
    // The acceptance case: nothing blocks on a failing endpoint. The plausible
    // bug is recording the key before the POST — the pass then looks
    // successful, the ledger says "reported", and the operator is never told.
    let mut store = store();
    let sink = RecordingSink::default();
    let view = bloom(BloomStatus::Sealed, vec![wedged_member("issue-1")]);

    sink.refuse(true);
    let refused = deliver(&mut store, &sink, &view, 1).expect("a refused POST is not a store failure");
    assert_eq!(refused.posted, 0);
    assert!(refused.stalled, "the pass reports that it stopped short");
    assert!(sink.posted().is_empty());
    assert!(store.list_notifications().expect("the ledger reads").is_empty(), "a refused message records nothing");

    sink.refuse(false);
    let retried = deliver(&mut store, &sink, &view, 2).expect("the ledger writes");
    assert_eq!(retried.posted, 2, "both the seal and the wedge are still owed");
    assert_eq!(sink.posted().len(), 2);
}

#[test]
fn a_cleared_condition_is_forgotten_and_notifies_again_if_it_returns() {
    // The plausible bug: the ledger is append-only, so a wedge that a grant
    // cleared and a later attempt re-earned is reported once ever — the
    // second stop is silent, which is the one an operator most needs.
    let mut store = store();
    let sink = RecordingSink::default();

    let wedged = bloom(BloomStatus::Sealed, vec![wedged_member("issue-1")]);
    deliver(&mut store, &sink, &wedged, 1).expect("the ledger writes");
    assert_eq!(sink.posted().len(), 2, "the seal and the wedge");

    let cleared = bloom(
        BloomStatus::Sealed,
        vec![MemberView { workpiece: WorkpieceId("issue-1".to_owned()), ..MemberView::default() }],
    );
    let quiet = deliver(&mut store, &sink, &cleared, 2).expect("the ledger writes");
    assert_eq!(quiet.forgotten, 1, "the wedge key is dropped once its condition clears");
    assert_eq!(sink.posted().len(), 2, "clearing a condition posts nothing");

    deliver(&mut store, &sink, &wedged, 3).expect("the ledger writes");
    assert_eq!(sink.posted().len(), 3, "the returning wedge is a new transition");
}
