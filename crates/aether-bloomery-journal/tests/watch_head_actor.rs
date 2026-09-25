//! `WatchHead`: a long-poll watch on the journal head (ADR-0226 decision 10).

mod actor_support;

use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use aether_actor::{ActorRef, ErasedActorRef};
use aether_bloomery_journal::{Batch, Journal, JournalActor, MAX_HEAD_WATCHERS, Seq};
use aether_bloomery_kinds::{
    AppendRecords, AppendRecordsResult, Digest, DriverRecord, Head, MoveHead, MoveHeadResult, NativeOrigin,
    ProgramName, ProgramRef, Publish, PublishResult, RecordedHeadMove, Ref, RequestSource, Requested, WatchHead,
    WatchHeadResult,
};
use aether_substrate::Subname;
use aether_substrate::chassis::builder::PassiveChassis;
use aether_substrate::mail::registry::{OwnedDispatch, Registry};
use aether_substrate::testing::{TestChassis, bare_substrate, boot_test_chassis_with};

use actor_support::{TestAnchor, caller, reply, request};

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.watch_head.note")]
struct Note {
    text: String,
}

const NOTES: Head<Note> = Head::new("notes");
const ALSO: Head<Note> = Head::new("also");

/// One journal actor over a root seeded with one stored note to move heads
/// to, and no events. `caller` issues writes and `watcher` issues watches on
/// its own mailbox, so a woken watch reply (sent synchronously inside the
/// committing write's own handler, before that handler's reply goes out)
/// never lands ahead of a write reply on the same channel.
struct Fixture {
    registry: Arc<Registry>,
    _chassis: PassiveChassis<TestChassis>,
    actor: ActorRef<JournalActor>,
    caller: ErasedActorRef,
    replies: mpsc::Receiver<OwnedDispatch>,
    watcher: ErasedActorRef,
    watcher_replies: mpsc::Receiver<OwnedDispatch>,
    note: Ref<Note>,
}

impl Fixture {
    fn start(path: &Path) -> Self {
        let note = {
            let mut journal = Journal::open(path).expect("create journal");
            let mut batch = Batch::new();
            let note = batch.stage_encoded(&Note { text: "stored".into() }).expect("stage note");
            journal.append(Seq(0), &batch).expect("seed artifact");
            note
        };

        let (registry, mailer) = bare_substrate();
        let (watcher, watcher_replies) = caller(&registry, "test.watch_head.watcher");
        let (caller, replies) = caller(&registry, "test.watch_head.caller");
        let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
        let actor =
            chassis.spawn_actor::<JournalActor>(Subname::Named("watch"), path.to_owned(), ()).finish().expect("birth");
        Self { registry, _chassis: chassis, actor, caller, replies, watcher, watcher_replies, note }
    }

    fn move_head(&self, correlation: u64, command: &MoveHead) -> MoveHeadResult {
        request(&self.registry, self.actor, self.caller, correlation, command);
        reply(&self.replies, correlation)
    }

    fn publish(&self, correlation: u64, command: &Publish) -> PublishResult {
        request(&self.registry, self.actor, self.caller, correlation, command);
        reply(&self.replies, correlation)
    }

    fn append_records(&self, correlation: u64, command: &AppendRecords) -> AppendRecordsResult {
        request(&self.registry, self.actor, self.caller, correlation, command);
        reply(&self.replies, correlation)
    }

    /// Send a watch from an arbitrary caller without waiting for its reply — it may park.
    fn send_watch(&self, caller: ErasedActorRef, correlation: u64, after: u64) {
        request(&self.registry, self.actor, caller, correlation, &WatchHead { after });
    }

    /// Send a watch from the fixture's own watcher, expected to answer at once (or immediately refuse).
    fn watch(&self, correlation: u64, after: u64) -> WatchHeadResult {
        self.send_watch(self.watcher, correlation, after);
        reply(&self.watcher_replies, correlation)
    }
}

/// The "still parked" check: no reply lands within a short wait.
fn no_reply(rx: &mpsc::Receiver<OwnedDispatch>) {
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "expected no reply, but one arrived");
}

/// One `Requested` record under `cause`, distinct per `key`.
fn requested(cause: Option<u64>, key: u64, expected_seq: u64) -> AppendRecords {
    let record = Requested {
        program: ProgramRef::new(
            Digest::from_bytes([1; 32]),
            ProgramName::new("bloomery.test.watch_head.program").expect("program name"),
        ),
        input: Digest::from_bytes([2; 32]),
        source: RequestSource::Native {
            origin: NativeOrigin::new("test.watch_head.driver").expect("native origin"),
            key,
        },
    };
    AppendRecords::new(Vec::new(), vec![DriverRecord::Requested { cause, record }], expected_seq)
}

#[test]
fn a_watch_behind_the_head_answers_at_once() {
    // Catches an off-by-one comparison (`>=` versus `>`) that either parks a
    // watch already passed or answers one the head has only reached.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal"));

    assert_eq!(fixture.move_head(1, &MoveHead::new(&NOTES, fixture.note, 0)), MoveHeadResult::Committed { seq: 1 });

    assert_eq!(fixture.watch(2, 0), WatchHeadResult::Advanced { head: 1 });
    fixture.send_watch(fixture.watcher, 3, 1);
    no_reply(&fixture.watcher_replies);
}

#[test]
fn a_parked_watch_wakes_on_commit_not_on_conflict_or_refusal() {
    // Catches waking on the conflict or error path, waking before the
    // append commits, and reporting the wrong head.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal"));
    assert_eq!(fixture.move_head(1, &MoveHead::new(&NOTES, fixture.note, 0)), MoveHeadResult::Committed { seq: 1 });

    fixture.send_watch(fixture.watcher, 2, 1);

    let stale = Publish::new(Vec::new(), vec![RecordedHeadMove::from(&NOTES.move_to(fixture.note))], 0);
    assert_eq!(fixture.publish(3, &stale), PublishResult::Conflict { actual: 1 });
    no_reply(&fixture.watcher_replies);

    let refused = fixture.append_records(4, &requested(Some(99), 1, 1));
    assert!(matches!(refused, AppendRecordsResult::Err { .. }), "out-of-fence cause must be refused: {refused:?}");
    no_reply(&fixture.watcher_replies);

    let moves =
        vec![RecordedHeadMove::from(&NOTES.move_to(fixture.note)), RecordedHeadMove::from(&ALSO.move_to(fixture.note))];
    let committed = fixture.publish(5, &Publish::new(Vec::new(), moves, 1));
    assert_eq!(committed, PublishResult::Committed { head: 3, artifacts: Vec::new() });

    assert_eq!(reply::<WatchHeadResult>(&fixture.watcher_replies, 2), WatchHeadResult::Advanced { head: 3 });
}

#[test]
fn each_parked_watch_is_answered_exactly_once() {
    // Catches a double answer, a lost co-waiter, and waking a watch the
    // head has not yet passed. Between them, the two parked-wake tests
    // drive all three writers through `commit`.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal"));
    assert_eq!(fixture.move_head(1, &MoveHead::new(&NOTES, fixture.note, 0)), MoveHeadResult::Committed { seq: 1 });

    let (caller_b, replies_b) = caller(&fixture.registry, "test.watch_head.caller_b");
    fixture.send_watch(fixture.watcher, 2, 1);
    fixture.send_watch(caller_b, 10, 1);
    fixture.send_watch(caller_b, 11, 5);

    let committed = fixture.append_records(3, &requested(None, 1, 1));
    assert_eq!(committed, AppendRecordsResult::Committed { head: 2, artifacts: Vec::new() });

    assert_eq!(reply::<WatchHeadResult>(&fixture.watcher_replies, 2), WatchHeadResult::Advanced { head: 2 });
    assert_eq!(reply::<WatchHeadResult>(&replies_b, 10), WatchHeadResult::Advanced { head: 2 });
    no_reply(&replies_b);

    assert_eq!(fixture.move_head(4, &MoveHead::new(&ALSO, fixture.note, 2)), MoveHeadResult::Committed { seq: 3 });
    no_reply(&fixture.watcher_replies);
    no_reply(&replies_b);
}

#[test]
fn a_full_watcher_table_refuses_instead_of_parking() {
    // Catches an unbounded table and a full table that parks or silently
    // drops the reply.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal"));

    for correlation in 1..=MAX_HEAD_WATCHERS as u64 {
        fixture.send_watch(fixture.watcher, correlation, 0);
    }

    let refused = fixture.watch(MAX_HEAD_WATCHERS as u64 + 1, 0);
    assert!(matches!(refused, WatchHeadResult::Err { .. }), "the watch beyond the bound must refuse: {refused:?}");

    assert_eq!(fixture.move_head(1000, &MoveHead::new(&NOTES, fixture.note, 0)), MoveHeadResult::Committed { seq: 1 });

    for correlation in 1..=MAX_HEAD_WATCHERS as u64 {
        assert_eq!(
            reply::<WatchHeadResult>(&fixture.watcher_replies, correlation),
            WatchHeadResult::Advanced { head: 1 }
        );
    }
    no_reply(&fixture.watcher_replies);
}
