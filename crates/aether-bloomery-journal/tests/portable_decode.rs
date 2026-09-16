//! Portable `Entry::decode` without the `SQLite` store.

use aether_bloomery_journal::{DecodeError, Entry, Seq};
use aether_data::{Invariant, Kind, Storage, StorageData, StorageError};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.note")]
struct Note {
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.other")]
struct Other {
    n: u64,
}

struct TooLong;

impl Invariant for TooLong {
    fn reason(&self) -> &'static str {
        "too-long"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
struct ShortName(String);

impl ShortName {
    fn check(inner: &str) -> Result<(), TooLong> {
        if inner.len() > 3 {
            Err(TooLong)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.checked_name")]
struct PlainName {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.journal.checked_name")]
struct CheckedName {
    name: ShortName,
}

fn encoded_entry<K: Storage + Clone>(value: &K) -> Entry {
    Entry {
        seq: Seq(1),
        kind: K::NAME.to_owned(),
        cause: None,
        recorded_at_millis: 0,
        bytes: K::encode_storage(&StorageData::from_value(value.clone())).expect("encode"),
    }
}

#[test]
fn decode_round_trips_a_supplied_entry() {
    let entry = encoded_entry(&Note { text: "hello".to_owned() });
    assert_eq!(entry.decode::<Note>().expect("decode note").text, "hello");
}

#[test]
fn decode_refuses_a_different_kind_name() {
    let entry = encoded_entry(&Note { text: "hello".to_owned() });
    let error = entry.decode::<Other>().expect_err("other kind must be refused");
    assert_eq!(error.to_string(), "entry kind \"test.journal.note\" is not \"test.journal.other\"");
    match error {
        DecodeError::KindMismatch { expected, actual } => {
            assert_eq!(expected, "test.journal.other");
            assert_eq!(actual, "test.journal.note");
        }
        DecodeError::Storage(other) => panic!("expected KindMismatch, got Storage({other:?})"),
    }
}

#[test]
fn decode_refuses_a_malformed_payload() {
    let entry =
        Entry { seq: Seq(1), kind: Note::NAME.to_owned(), cause: None, recorded_at_millis: 0, bytes: vec![0xff, 0x00] };
    let error = entry.decode::<Note>().expect_err("malformed payload must fail");
    match &error {
        DecodeError::Storage(_) => {
            let rendered = error.to_string();
            assert!(rendered.starts_with("failed to decode entry:"), "{rendered}");
        }
        DecodeError::KindMismatch { expected, actual } => {
            panic!("expected Storage, got KindMismatch {{ expected: {expected:?}, actual: {actual:?} }}")
        }
    }
}

#[test]
fn decode_refuses_a_payload_that_breaks_a_checked_invariant() {
    let entry = encoded_entry(&PlainName { name: "abcd".to_owned() });
    let error = entry.decode::<CheckedName>().expect_err("invariant must fail");
    match error {
        DecodeError::Storage(StorageError::Invariant { kind, reason }) => {
            assert_eq!(kind, "ShortName");
            assert_eq!(reason, "too-long");
        }
        other => panic!("expected Storage Invariant, got {other:?}"),
    }
}
