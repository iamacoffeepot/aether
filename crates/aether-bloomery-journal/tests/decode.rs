//! Decode round-trips the stored kind and refuses a different kind name.

mod common;

use std::error::Error;

use aether_bloomery_journal::{DecodeError, Journal, Seq};
use common::{Note, Other, journal};

#[test]
fn decode_round_trips_the_appended_kind_and_refuses_a_different_kind_name() -> Result<(), Box<dyn Error>> {
    let mut journal = journal()?;
    journal.append(Seq(0), &[Note::draft("hello")])?;
    let entry = journal.read(Seq(0), 1)?.into_iter().next().expect("one entry");

    assert_eq!(Journal::decode::<Note>(&entry)?.text, "hello");

    let error = Journal::decode::<Other>(&entry).expect_err("other kind must be refused");
    match error {
        DecodeError::KindMismatch { expected, actual } => {
            assert_eq!(expected, "test.journal.other");
            assert_eq!(actual, "test.journal.note");
        }
        other => panic!("expected KindMismatch, got {other:?}"),
    }
    Ok(())
}
