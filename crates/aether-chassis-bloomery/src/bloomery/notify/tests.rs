//! Store-backed tests for the notification ledger (#5166).
//!
//! Everything here drives [`deliver`] against a real `:memory:` store and a
//! recording sink, because the logic under test *is* the difference between a
//! document's loud set and the ledger — a fake store would be testing the
//! fake.

use std::sync::{Mutex, PoisonError};

use aether_bloomery::testing::digest;
use aether_bloomery::{
    AwaitingSurfaceView, BloomId, BloomStatus, BloomView, CompositionFinding, CompositionView, ExecutorFaultView,
    HostFaultView, LandingBlock, MemberPark, MemberView, OperatorHold, PendingDecisionView, ReviewParkView,
    SpendQuiesce, StageId, SurfacePathRequest, VerifyFailureSet, ViewDocument, Wedge, WorkpieceId,
};
use aether_bloomery_github::{WebhookError, WebhookSink};

use super::runtime::SEED_MARKER_KEY;
use super::{deliver, loud_events};
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

fn standing_loud_set() -> ViewDocument {
    ViewDocument {
        blooms: vec![
            BloomView {
                id: BloomId(digest(0xab)),
                status: BloomStatus::Landed,
                members: vec![wedged_member("issue-1")],
                ..BloomView::default()
            },
            BloomView {
                id: BloomId(digest(0xcd)),
                status: BloomStatus::Superseded,
                superseded_by: Some(BloomId(digest(0xef))),
                ..BloomView::default()
            },
        ],
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
    deliver(&mut store, &sink, &ViewDocument::default(), 0).expect("the first mount seeds");

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
    deliver(&mut store, &sink, &ViewDocument::default(), 0).expect("the first mount seeds");
    let view = bloom(BloomStatus::Sealed, vec![wedged_member("issue-1")]);

    sink.refuse(true);
    let refused = deliver(&mut store, &sink, &view, 1).expect("a refused POST is not a store failure");
    assert_eq!(refused.posted, 0);
    assert!(refused.stalled, "the pass reports that it stopped short");
    assert!(sink.posted().is_empty());
    assert_eq!(
        store.list_notifications().expect("the ledger reads"),
        [SEED_MARKER_KEY],
        "a refused message records nothing",
    );

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
    deliver(&mut store, &sink, &ViewDocument::default(), 0).expect("the first mount seeds");

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

#[test]
fn a_first_mount_adopts_the_standing_loud_set_without_posting() {
    // The bug: an empty ledger used to mean "nothing has ever been reported",
    // which on first mount is indistinguishable from "everything currently
    // loud is a fresh transition" — so the first pass posted the whole day's
    // history. The plausible remaining bug is posting those keys instead of
    // recording them.
    let mut store = store();
    let sink = RecordingSink::default();
    let standing = standing_loud_set();
    let events = loud_events(&standing);

    let report = deliver(&mut store, &sink, &standing, 1).expect("the ledger writes");
    assert_eq!(report.posted, 0);
    assert_eq!(report.seeded, u32::try_from(events.len()).expect("the standing set is tiny"));
    assert!(sink.posted().is_empty());

    let mut expected: Vec<_> = events.into_iter().map(|event| event.key).collect();
    expected.push(SEED_MARKER_KEY.to_owned());
    expected.sort();
    assert_eq!(store.list_notifications().expect("the ledger reads"), expected);
}

#[test]
fn a_transition_after_the_seed_still_posts_exactly_once() {
    // Names the bug the seeding could introduce: a seed that swallowed later
    // transitions would make the channel permanently silent.
    let mut store = store();
    let sink = RecordingSink::default();
    let standing = standing_loud_set();
    deliver(&mut store, &sink, &standing, 1).expect("the first mount seeds");
    assert!(sink.posted().is_empty());

    let mut blooms = standing.blooms.clone();
    blooms.push(BloomView {
        id: BloomId(digest(0x11)),
        status: BloomStatus::Sealed,
        members: vec![MemberView { workpiece: WorkpieceId("issue-2".to_owned()), ..MemberView::default() }],
        ..BloomView::default()
    });
    let with_new = ViewDocument { blooms, ..ViewDocument::default() };
    let first = deliver(&mut store, &sink, &with_new, 2).expect("the ledger writes");
    assert_eq!(first.posted, 1);
    assert_eq!(sink.posted().len(), 1);

    let second = deliver(&mut store, &sink, &with_new, 3).expect("the ledger writes");
    assert_eq!(second.posted, 0);
    assert_eq!(sink.posted().len(), 1);
}

#[test]
fn an_emptied_ledger_is_not_re_seeded() {
    // Tripwire: without the marker the ledger reads as empty here and the
    // returning condition is seeded into silence — the pre-fix failure mode,
    // inverted.
    let mut store = store();
    let sink = RecordingSink::default();
    deliver(&mut store, &sink, &standing_loud_set(), 1).expect("the first mount seeds");

    let quiet = deliver(&mut store, &sink, &ViewDocument::default(), 2).expect("the ledger writes");
    assert_eq!(store.list_notifications().expect("the ledger reads"), [SEED_MARKER_KEY]);
    assert_eq!(quiet.seeded, 0, "an emptied ledger must not re-arm the seed");
    assert!(sink.posted().is_empty());

    let sealed = bloom(
        BloomStatus::Sealed,
        vec![MemberView { workpiece: WorkpieceId("issue-1".to_owned()), ..MemberView::default() }],
    );
    let report = deliver(&mut store, &sink, &sealed, 3).expect("the ledger writes");
    assert_eq!(report.posted, 1);
    assert_eq!(sink.posted().len(), 1);
}

#[test]
fn the_seed_marker_is_never_posted_and_never_forgotten() {
    let mut store = store();
    let sink = RecordingSink::default();

    deliver(&mut store, &sink, &standing_loud_set(), 1).expect("the first mount seeds");
    deliver(&mut store, &sink, &ViewDocument::default(), 2).expect("the ledger writes");
    deliver(&mut store, &sink, &bloom(BloomStatus::Sealed, vec![]), 3).expect("the ledger writes");
    deliver(&mut store, &sink, &bloom(BloomStatus::Landed, vec![wedged_member("issue-1")]), 4)
        .expect("the ledger writes");

    let posted = sink.posted();
    assert!(
        posted.iter().all(|message| !message.contains(SEED_MARKER_KEY)),
        "the marker is never a posted message: {posted:?}"
    );
    assert!(
        store.list_notifications().expect("the ledger reads").iter().any(|key| key == SEED_MARKER_KEY),
        "the marker must survive every forget sweep"
    );
}

fn every_loud_branch_keys() -> Vec<String> {
    let sink_member = MemberView {
        workpiece: WorkpieceId("issue-1".to_owned()),
        wedge: Some(Wedge {
            stage: StageId::Verify,
            evidence: digest(9),
            repeated_verifiers: VerifyFailureSet::default(),
        }),
        host_fault: Some(HostFaultView { findings: "no cargo".to_owned() }),
        pending_decision: Some(PendingDecisionView {
            question: digest(4),
            stage: StageId::Review,
            prompt: "grant?".to_owned(),
            options: vec!["yes".to_owned()],
            blocked: "dispatch".to_owned(),
        }),
        park: Some(MemberPark { stage: StageId::Construct, evidence: digest(3) }),
        awaiting_surface: Some(AwaitingSurfaceView {
            stage: StageId::Construct,
            scope_revision: digest(5),
            evidence: digest(6),
            paths: vec![SurfacePathRequest { path: "crates/x.rs".to_owned(), reason: "needed".to_owned() }],
            summary: "surface too tight".to_owned(),
            requests: 1,
        }),
        ..MemberView::default()
    };
    let kitchen = ViewDocument {
        spend_quiesce: Some(SpendQuiesce::Window {
            window: "today".to_owned(),
            spent_micro_usd: 1,
            ceiling_micro_usd: 1,
        }),
        blooms: vec![
            BloomView {
                id: BloomId(digest(0xab)),
                status: BloomStatus::Sealed,
                review_park: Some(ReviewParkView { question: digest(7), ..ReviewParkView::default() }),
                landing_blocked: Some(LandingBlock { rolls: 2, budget: 3 }),
                executor_fault: Some(ExecutorFaultView {
                    subject: digest(1),
                    rolls: 1,
                    budget: 3,
                    evidence: digest(2),
                    terminal: true,
                }),
                operator_hold: Some(OperatorHold { reason: "freeze".to_owned(), operator: "owner".to_owned() }),
                composition: Some(CompositionView {
                    findings: vec![CompositionFinding { subject: digest(8), detail: digest(9), implicated: vec![] }],
                    wedge: Some(Wedge {
                        stage: StageId::Verify,
                        evidence: digest(10),
                        repeated_verifiers: VerifyFailureSet::default(),
                    }),
                    ..CompositionView::default()
                }),
                members: vec![sink_member],
                ..BloomView::default()
            },
            BloomView { id: BloomId(digest(0x11)), status: BloomStatus::Landed, ..BloomView::default() },
            BloomView {
                id: BloomId(digest(0x22)),
                status: BloomStatus::Superseded,
                superseded_by: Some(BloomId(digest(0x33))),
                ..BloomView::default()
            },
            BloomView { id: BloomId(digest(0x44)), status: BloomStatus::Withdrawn, ..BloomView::default() },
        ],
        ..ViewDocument::default()
    };
    let bloom_quiesce = ViewDocument {
        spend_quiesce: Some(SpendQuiesce::Bloom {
            window: "today".to_owned(),
            bloom: BloomId(digest(0xab)),
            spent_micro_usd: 1,
            ceiling_micro_usd: 1,
        }),
        ..ViewDocument::default()
    };

    let mut keys: Vec<_> = loud_events(&kitchen).into_iter().map(|event| event.key).collect();
    keys.extend(loud_events(&bloom_quiesce).into_iter().map(|event| event.key));
    keys
}

#[test]
fn no_loud_event_key_collides_with_the_seed_marker() {
    // Tripwire: a taxonomy key that ever collided would be a condition the
    // reactor silently refuses to report.
    let keys = every_loud_branch_keys();
    assert!(
        keys.iter().any(|key| key.starts_with("quiesce:window:")),
        "window spend quiesce must be in the document: {keys:?}"
    );
    assert!(
        keys.iter().any(|key| key.starts_with("quiesce:bloom:")),
        "bloom spend quiesce must be in the document: {keys:?}"
    );
    assert!(keys.iter().any(|key| key.ends_with(":Sealed")), "sealed status: {keys:?}");
    assert!(keys.iter().any(|key| key.ends_with(":Landed")), "landed status: {keys:?}");
    assert!(keys.iter().any(|key| key.ends_with(":Superseded")), "superseded status: {keys:?}");
    assert!(keys.iter().any(|key| key.ends_with(":Withdrawn")), "withdrawn status: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("park:")), "review park: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("landing:")), "landing block: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("fault:")), "executor fault: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("hold:")), "operator hold: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("findings:")), "composition findings: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("composition_wedge:")), "composition wedge: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("wedge:")), "member wedge: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("host_fault:")), "member host fault: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("decision:")), "member decision: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("member_park:")), "member park: {keys:?}");
    assert!(keys.iter().any(|key| key.starts_with("surface:")), "member surface: {keys:?}");

    for key in &keys {
        assert_ne!(key.as_str(), SEED_MARKER_KEY);
        assert!(!key.starts_with(SEED_MARKER_KEY), "taxonomy key {key} collides with the seed marker");
    }
}
