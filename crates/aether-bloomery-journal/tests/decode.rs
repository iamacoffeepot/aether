//! Decode round-trips the stored kind and refuses a different kind name.

mod common;

use std::error::Error;

use aether_bloomery_journal::{DecodeError, Draft, Journal, Seq};
use common::{FixedClock, batch_from_drafts};

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
fn decode_round_trips_the_appended_kind_and_refuses_a_different_kind_name() -> Result<(), Box<dyn Error>> {
    let mut journal = Journal::open_in_memory_with_clock(Box::new(FixedClock(0)))?;
    journal.append(Seq(0), &batch_from_drafts([Note::draft("hello")]))?;
    let entry = journal.read(Seq(0), 1)?.into_iter().next().expect("one entry");

    assert_eq!(Journal::decode::<Note>(&entry)?.text, "hello");

    let error = Journal::decode::<Other>(&entry).expect_err("other kind must be refused");
    match error {
        DecodeError::KindMismatch { expected, actual } => {
            assert_eq!(expected, "test.journal.other");
            assert_eq!(actual, "test.journal.note");
        }
        DecodeError::Storage(other) => panic!("expected KindMismatch, got Storage({other:?})"),
    }
    Ok(())
}
