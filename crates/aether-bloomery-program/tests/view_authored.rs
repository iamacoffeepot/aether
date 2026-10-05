//! Semantic coverage for typed aggregate view authoring.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use aether_bloomery_kinds::{ClosureArtifact, Entry, Head, HeadMoved, Program, Seq, Tree};
use aether_bloomery_program::{Cited, CitedError, SequenceError, View, ViewCursor, ViewFoldError, view};
use aether_data::{Digest, Kind, OpaqueBytes, Ref, Storage, StorageData};

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.note")]
struct Note {
    key: u64,
    fail: bool,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Moves {
    cursor: ViewCursor,
    counts: BTreeMap<String, u64>,
    order: Vec<&'static str>,
}

#[view(cursor = cursor)]
impl View for Moves {
    #[fold]
    fn count(&mut self, event: HeadMoved<Tree>) {
        if event.head().as_str() == "filtered" {
            return;
        }
        *self.counts.entry(event.head().as_str().to_owned()).or_default() += 1;
        self.order.push("count");
    }

    #[fold]
    fn observe(&mut self, _event: HeadMoved<Tree>) {
        self.order.push("observe");
    }
}

#[derive(Debug)]
struct Boom(u64);

impl fmt::Display for Boom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "boom {}", self.0)
    }
}

impl Error for Boom {}

#[derive(Default)]
struct Fallible {
    cursor: ViewCursor,
    accepted: Vec<u64>,
}

#[view(cursor = cursor)]
impl View for Fallible {
    #[fold]
    fn note(&mut self, event: Note) -> Result<(), Boom> {
        if event.fail {
            return Err(Boom(event.key));
        }
        self.accepted.push(event.key);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.view.cites_note")]
struct CitesNote {
    note: Ref<Note>,
}

#[derive(Default)]
struct CitedNotes {
    cursor: ViewCursor,
    keys: Vec<u64>,
}

#[view(cursor = cursor)]
impl View for CitedNotes {
    #[fold]
    fn read(&mut self, event: CitesNote, cited: &Cited) -> Result<(), CitedError> {
        self.keys.push(cited.get(event.note)?.key);
        Ok(())
    }
}

/// One stored `Note` as the driver delivers it: its ref and its artifact.
fn note_artifact(key: u64) -> (Ref<Note>, ClosureArtifact) {
    let note = Note { key, fail: false };
    let payload = Note::encode_storage(&StorageData::from_value(note.clone())).expect("storage encode");
    (Ref::of_encoded(&note).expect("digest the note"), ClosureArtifact::new(Note::ID, payload))
}

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn entry_for<K: Storage + Clone>(seq: u64, event: &K) -> Entry {
    Entry {
        seq: Seq(seq),
        kind: K::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(event.clone())).expect("storage encode"),
    }
}

fn moved<K: Kind + 'static>(seq: u64, name: &'static str, byte: u8) -> Entry {
    entry_for(seq, &Head::<K>::new(name).move_to(digest_ref(byte)))
}

fn unrelated(seq: u64) -> Entry {
    entry_for(seq, &Note { key: seq, fail: false })
}

#[test]
fn aggregate_keys_order_filtering_and_mismatches_share_one_cursor() {
    let mut view = Moves::empty();
    assert_eq!(view.cursor(), Seq(0));

    view.advance(&[
        moved::<Program>(1, "program", 1),
        unrelated(2),
        moved::<Tree>(3, "alpha", 2),
        moved::<Tree>(4, "beta", 3),
        moved::<Tree>(5, "alpha", 4),
        moved::<Tree>(6, "filtered", 5),
    ])
    .expect("fold");

    assert_eq!(view.cursor(), Seq(6));
    assert_eq!(view.counts.get("alpha"), Some(&2));
    assert_eq!(view.counts.get("beta"), Some(&1));
    assert_eq!(view.counts.get("filtered"), None);
    assert_eq!(view.order, ["count", "observe", "count", "observe", "count", "observe", "observe"]);
}

#[test]
fn replay_is_equivalent_across_batch_boundaries() {
    let entries =
        [unrelated(1), moved::<Tree>(2, "alpha", 1), moved::<Program>(3, "program", 2), moved::<Tree>(4, "beta", 3)];
    let mut whole = Moves::empty();
    whole.advance(&entries).expect("whole");

    let mut chunked = Moves::empty();
    chunked.advance(&entries[..1]).expect("first");
    chunked.advance(&entries[1..3]).expect("middle");
    chunked.advance(&entries[3..]).expect("last");
    chunked.advance(&[]).expect("empty batch");

    assert_eq!(whole, chunked);
}

#[test]
fn malformed_matching_payload_does_not_advance() {
    let mut view = Moves::empty();
    view.advance(&[moved::<Tree>(1, "alpha", 1)]).expect("first");
    let malformed =
        Entry { seq: Seq(2), kind: HeadMoved::<Tree>::ID, cause: None, recorded_at_millis: 0, bytes: Vec::new() };

    let error = view.advance(&[malformed]).expect_err("malformed matching event");
    assert_eq!(view.cursor(), Seq(1));
    assert_eq!(view.counts.get("alpha"), Some(&1));
    assert_eq!(error.handler_name(), Some("count"));
    assert!(matches!(error, ViewFoldError::Decode { .. }));
    assert!(error.source().is_some());
}

#[test]
fn sequence_failures_are_distinguished() {
    let mut duplicate = Moves::empty();
    duplicate.advance(&[unrelated(1)]).expect("first");
    assert!(matches!(
        duplicate.advance(&[unrelated(1)]),
        Err(ViewFoldError::Sequence(SequenceError::Duplicate { .. }))
    ));

    let mut gap = Moves::empty();
    assert!(matches!(gap.advance(&[unrelated(2)]), Err(ViewFoldError::Sequence(SequenceError::Gap { .. }))));

    let mut backwards = Moves::empty();
    backwards.advance(&[unrelated(1), unrelated(2)]).expect("prefix");
    assert!(matches!(
        backwards.advance(&[unrelated(1)]),
        Err(ViewFoldError::Sequence(SequenceError::Backwards { .. }))
    ));
}

#[test]
fn handler_error_preserves_source_and_previous_entries() {
    let mut view = Fallible::empty();
    let error = view
        .advance(&[entry_for(1, &Note { key: 10, fail: false }), entry_for(2, &Note { key: 20, fail: true })])
        .expect_err("handler fails");

    assert_eq!(view.cursor(), Seq(1));
    assert_eq!(view.accepted, [10]);
    assert_eq!(error.handler_name(), Some("note"));
    let source = error.source().expect("handler source").downcast_ref::<Boom>().expect("boom");
    assert_eq!(source.0, 20);
}

#[test]
fn a_fold_reads_only_what_its_own_entry_cites() {
    // Catches a fold handed the page's pooled artifacts rather than its own
    // entry's, which would let it read a digest only a neighbour cites.
    let (first, first_artifact) = note_artifact(10);
    let (second, second_artifact) = note_artifact(20);
    let pool = [first_artifact, second_artifact];
    let entries = [entry_for(1, &CitesNote { note: first }), entry_for(2, &CitesNote { note: second })];
    let cited = [Cited::new(vec![first.digest()], &pool), Cited::new(vec![second.digest()], &pool)];

    let mut view = CitedNotes::empty();
    view.advance_cited(&entries, &cited).expect("each entry reads its own citation");
    assert_eq!(view.keys, [10, 20]);

    let mut crossed = CitedNotes::empty();
    let reads_neighbour = [entry_for(1, &CitesNote { note: second })];
    let error = crossed
        .advance_cited(&reads_neighbour, &[Cited::new(vec![first.digest()], &pool)])
        .expect_err("the entry does not cite the digest it names");
    assert_eq!(crossed.cursor(), Seq(0));
    let source = error.source().expect("handler source").downcast_ref::<CitedError>().expect("cited error");
    assert!(matches!(source, CitedError::NotCited { digest } if *digest == second.digest()), "{source:?}");
}

#[test]
fn a_cited_artifact_of_another_kind_fails_the_fold() {
    // Catches a read that decodes whatever bytes sit under the digest
    // without checking they are the kind the ref names.
    let bytes = ClosureArtifact::new(OpaqueBytes::ID, b"not a note".to_vec());
    let digest = bytes.claimed().unverified();
    let entry = entry_for(1, &CitesNote { note: Ref::from_digest(digest) });

    let mut view = CitedNotes::empty();
    let error = view.advance_cited(&[entry], &[Cited::new(vec![digest], &[bytes])]).expect_err("kind mismatch");
    assert_eq!(view.cursor(), Seq(0));
    assert_eq!(error.handler_name(), Some("read"));
    let source = error.source().expect("handler source").downcast_ref::<CitedError>().expect("cited error");
    assert!(
        matches!(source, CitedError::KindMismatch { expected, actual, .. } if *expected == Note::ID && *actual == OpaqueBytes::ID),
        "{source:?}"
    );
}
