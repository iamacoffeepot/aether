//! Fenced write mail: a pure head move over stored content and an atomic publish of artifacts plus moves.

mod actor_support;

use std::path::Path;
use std::sync::{Arc, mpsc};

use aether_actor::ActorRef;
use aether_bloomery_journal::{Batch, Journal, JournalActor, Ref, Seq};
use aether_bloomery_kinds::{
    Digest, EncodedArtifact, Head, MoveHead, MoveHeadResult, Publish, PublishResult, RecordedHeadMove,
};
use aether_data::MailboxId;
use aether_substrate::Subname;
use aether_substrate::chassis::builder::PassiveChassis;
use aether_substrate::mail::registry::{OwnedDispatch, Registry};
use aether_substrate::testing::{TestChassis, bare_substrate, boot_test_chassis_with};

use actor_support::{TestAnchor, caller, reply, request};

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.journal_write.note")]
struct Note {
    text: String,
}

#[derive(Clone, Debug, aether_data::Storage)]
#[kind(name = "test.bloomery.journal_write.marker")]
struct Marker {
    value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.journal_write.linked_note")]
struct LinkedNote {
    title: String,
    note: Ref<Note>,
}

const NOTES: Head<Note> = Head::new("notes");
const MAIN: Head<LinkedNote> = Head::new("main");

/// One journal actor over a file seeded with a stored note and a stored marker, and no events.
struct Fixture {
    registry: Arc<Registry>,
    _chassis: PassiveChassis<TestChassis>,
    actor: ActorRef<JournalActor>,
    caller: MailboxId,
    replies: mpsc::Receiver<OwnedDispatch>,
    journal: Journal,
    note: Ref<Note>,
    marker: Digest,
}

impl Fixture {
    fn start(path: &Path) -> Self {
        let (note, marker) = {
            let mut journal = Journal::open(path).expect("create journal");
            let mut batch = Batch::new();
            let note = batch.stage_encoded(&Note { text: "stored".into() }).expect("stage note");
            let marker = batch.stage_encoded(&Marker { value: 9 }).expect("stage marker");
            journal.append(Seq(0), &batch).expect("seed artifacts");
            (note, marker.digest())
        };

        let (registry, mailer) = bare_substrate();
        let (caller, replies) = caller(&registry, "test.journal_write.caller");
        let chassis = boot_test_chassis_with::<TestAnchor>(&registry, &mailer, (), ());
        let actor =
            chassis.spawn_actor::<JournalActor>(Subname::Named("writes"), path.to_owned(), ()).finish().expect("birth");
        let journal = Journal::open(path).expect("observe journal");
        Self { registry, _chassis: chassis, actor, caller, replies, journal, note, marker }
    }

    fn move_head(&self, correlation: u64, command: &MoveHead) -> MoveHeadResult {
        request(&self.registry, self.actor, self.caller, correlation, command);
        reply(&self.replies, correlation)
    }

    fn publish(&self, correlation: u64, command: &Publish) -> PublishResult {
        request(&self.registry, self.actor, self.caller, correlation, command);
        reply(&self.replies, correlation)
    }

    fn head(&self) -> Seq {
        self.journal.head().expect("journal head")
    }

    fn moves(&self) -> Vec<RecordedHeadMove> {
        self.journal
            .read(Seq(0), 128)
            .expect("read events")
            .iter()
            .map(|entry| {
                assert_eq!(entry.cause, None, "write mail must not record a cause");
                entry.decode::<RecordedHeadMove>().expect("decode head move")
            })
            .collect()
    }

    fn stored(&self, digest: &Digest) -> bool {
        self.journal.get_bytes(digest).expect("read artifact").is_some()
    }
}

fn linked(title: &str, note: Ref<Note>) -> LinkedNote {
    LinkedNote { title: title.into(), note }
}

#[test]
fn move_head_points_at_stored_content_and_refuses_missing_or_wrong_kind_destinations() {
    // Catches a head move that re-staged content or skipped the journal's
    // destination check, so a head could point at nothing or at another kind.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal.sqlite"));

    assert_eq!(fixture.move_head(1, &MoveHead::new(&NOTES, fixture.note, 0)), MoveHeadResult::Committed { seq: 1 });
    assert_eq!(fixture.head(), Seq(1));
    assert_eq!(fixture.moves(), [RecordedHeadMove::from(&NOTES.move_to(fixture.note))]);

    for (correlation, to) in [(2, Digest::from_bytes([5; 32])), (3, fixture.marker)] {
        let refused = fixture.move_head(correlation, &MoveHead::new(&NOTES, Ref::from_digest(to), 1));
        assert!(matches!(refused, MoveHeadResult::Err { .. }), "destination {to:?} must be refused: {refused:?}");
        assert_eq!(fixture.head(), Seq(1));
    }
}

#[test]
fn stale_fences_write_nothing() {
    // Catches a handler that appended without the whole-journal fence, or
    // staged a publish's artifacts before the fence was judged.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal.sqlite"));
    let event = NOTES.move_to(fixture.note);
    assert_eq!(fixture.move_head(1, &MoveHead::from_event(&event, 0)), MoveHeadResult::Committed { seq: 1 });

    assert_eq!(fixture.move_head(2, &MoveHead::from_event(&event, 0)), MoveHeadResult::Conflict { actual: 1 });
    let stale = Publish::head(&MAIN, &linked("stale", fixture.note), 0).expect("encode stale publish");
    assert_eq!(fixture.publish(3, &stale), PublishResult::Conflict { actual: 1 });
    assert_eq!(fixture.head(), Seq(1));
    assert_eq!(fixture.moves().len(), 1);
    assert!(!fixture.stored(&stale.artifacts()[0].digest()));
}

#[test]
fn publish_stages_artifacts_citing_each_other_and_moves_heads_atomically() {
    // Catches a publish that judged each artifact alone (so an in-batch
    // citation dangled), reported the wrong head for the appended moves, or
    // returned digests out of request order.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal.sqlite"));
    let note = EncodedArtifact::new(&Note { text: "fresh".into() }).expect("encode note");
    let note_ref = Ref::<Note>::from_digest(note.digest());
    let value = linked("published", note_ref);
    let document = EncodedArtifact::new(&value).expect("encode linked note");
    let document_ref = Ref::<LinkedNote>::from_digest(document.digest());
    let moves =
        vec![RecordedHeadMove::from(&NOTES.move_to(note_ref)), RecordedHeadMove::from(&MAIN.move_to(document_ref))];

    let committed = fixture.publish(1, &Publish::new(vec![note, document], moves.clone(), 0));
    assert_eq!(
        committed,
        PublishResult::Committed { head: 2, artifacts: vec![note_ref.digest(), document_ref.digest()] }
    );
    assert_eq!(fixture.head(), Seq(2));
    assert_eq!(fixture.moves(), moves);
    assert_eq!(fixture.journal.get::<LinkedNote>(&document_ref.digest()).expect("read document"), Some(value));
    assert!(fixture.stored(&note_ref.digest()));
}

#[test]
fn artifact_only_publish_stores_content_without_events() {
    // Catches a head computed from an empty event range as `head + 1`.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal.sqlite"));
    let artifact = EncodedArtifact::new(&linked("unpointed", fixture.note)).expect("encode artifact");
    let digest = artifact.digest();

    assert_eq!(
        fixture.publish(1, &Publish::new(vec![artifact], Vec::new(), 0)),
        PublishResult::Committed { head: 0, artifacts: vec![digest] }
    );
    assert_eq!(fixture.head(), Seq(0));
    assert!(fixture.stored(&digest));
}

#[test]
fn publish_with_a_bad_citation_rolls_back_every_artifact_and_move() {
    // Catches a publish that committed its valid artifacts or moves when one
    // artifact cited a missing or wrong-kind digest.
    let temp = tempfile::tempdir().expect("temporary journal directory");
    let fixture = Fixture::start(&temp.path().join("journal.sqlite"));

    for (correlation, cited) in [(1, Digest::from_bytes([6; 32])), (2, fixture.marker)] {
        let good = EncodedArtifact::new(&Note { text: format!("good-{correlation}") }).expect("encode good note");
        let good_ref = Ref::<Note>::from_digest(good.digest());
        let bad = EncodedArtifact::new(&linked("bad", Ref::from_digest(cited))).expect("encode bad document");
        let bad_digest = bad.digest();
        let publish = Publish::new(vec![good, bad], vec![RecordedHeadMove::from(&NOTES.move_to(good_ref))], 0);

        let refused = fixture.publish(correlation, &publish);
        assert!(matches!(refused, PublishResult::Err { .. }), "citation {cited:?} must be refused: {refused:?}");
        assert_eq!(fixture.head(), Seq(0));
        assert!(!fixture.stored(&good_ref.digest()));
        assert!(!fixture.stored(&bad_digest));
    }
}
