//! Decode round-trips the stored kind and refuses a different kind id.

mod common;

use std::error::Error;

use aether_bloomery_journal::{Batch, DecodeError, Draft, Journal, Seq};
use aether_data::Kind;

fn batch_from_drafts(drafts: impl IntoIterator<Item = Draft>) -> Batch {
    let mut batch = Batch::new();
    for draft in drafts {
        batch.push_draft(draft);
    }
    batch
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.note")]
struct Note {
    text: String,
}

impl Note {
    fn draft(text: &str) -> Draft {
        Draft::of(&Self { text: text.to_owned() }, None).expect("encode note")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.other")]
struct Other {
    n: u64,
}

#[test]
fn decode_round_trips_the_appended_kind_and_refuses_a_different_kind_id() -> Result<(), Box<dyn Error>> {
    let (_root, mut journal) = common::temp_journal(0)?;
    journal.append(Seq(0), &batch_from_drafts([Note::draft("hello")]))?;
    let entry = journal.read(Seq(0), 1)?.into_iter().next().expect("one entry");

    assert_eq!(Journal::decode::<Note>(&entry)?.text, "hello");

    let error = Journal::decode::<Other>(&entry).expect_err("other kind must be refused");
    match error {
        DecodeError::KindMismatch { expected, actual } => {
            assert_eq!(expected, Other::ID);
            assert_eq!(actual, Note::ID);
        }
        DecodeError::SpecializationMismatch { expected, actual } => {
            panic!("expected KindMismatch, got SpecializationMismatch {{ expected: {expected}, actual: {actual} }}")
        }
        DecodeError::Storage(other) => panic!("expected KindMismatch, got Storage({other:?})"),
    }
    Ok(())
}
