//! Portable `Entry::decode` without the journal store.

use aether_bloomery_kinds::{DecodeError, Digest, Entry, Head, HeadMoved, Program, Ref, Seq, Tree};
use aether_data::{Invariant, Kind, Storage, StorageData, StorageError};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.kinds.note")]
struct Note {
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.kinds.other")]
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
#[kind(name = "test.kinds.checked_name")]
struct PlainName {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.kinds.checked_name")]
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
    assert_eq!(error.to_string(), "entry kind \"test.kinds.note\" is not \"test.kinds.other\"");
    match error {
        DecodeError::KindMismatch { expected, actual } => {
            assert_eq!(expected, "test.kinds.other");
            assert_eq!(actual, "test.kinds.note");
        }
        DecodeError::SpecializationMismatch { expected, actual } => {
            panic!("expected KindMismatch, got SpecializationMismatch {{ expected: {expected}, actual: {actual} }}")
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
        DecodeError::SpecializationMismatch { expected, actual } => {
            panic!("expected Storage, got SpecializationMismatch {{ expected: {expected}, actual: {actual} }}")
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

#[test]
fn decode_skips_a_well_formed_head_moved_specialization() {
    let event = Head::<Program>::new("current").move_to(Ref::from_digest(Digest::from_bytes([1; 32])));
    let entry = encoded_entry(&event);
    let error = entry.decode::<HeadMoved<Tree>>().expect_err("program move is not a tree trigger");
    assert!(error.is_unmatched(), "{error}");
    match error {
        DecodeError::SpecializationMismatch { expected, actual } => {
            assert_eq!(expected, Tree::ID);
            assert_eq!(actual, Program::ID);
        }
        other => panic!("expected SpecializationMismatch, got {other:?}"),
    }
    assert_eq!(
        entry.decode::<HeadMoved<Program>>().expect("matching specialization").to().digest(),
        event.to().digest()
    );
}

#[test]
fn decode_refuses_a_malformed_matching_head_moved() {
    let entry = Entry {
        seq: Seq(1),
        kind: HeadMoved::<Tree>::NAME.to_owned(),
        cause: None,
        recorded_at_millis: 0,
        bytes: vec![0xff, 0x00],
    };
    let error = entry.decode::<HeadMoved<Tree>>().expect_err("malformed payload must fail");
    assert!(!error.is_unmatched(), "{error}");
    match error {
        DecodeError::Storage(_) => {}
        other => panic!("expected Storage, got {other:?}"),
    }
}

#[test]
fn decode_refuses_a_malformed_head_moved_of_another_specialization() {
    #[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
    #[kind(name = "bloomery.head_moved")]
    struct Twin {
        target_kind: aether_data::KindId,
        head: String,
        to: Digest,
    }

    let twin = Twin { target_kind: Program::ID, head: " ".into(), to: Digest::from_bytes([1; 32]) };
    let entry = encoded_entry(&twin);
    let error = entry.decode::<HeadMoved<Tree>>().expect_err("invalid name is not an unmatched trigger");
    assert!(!error.is_unmatched(), "{error}");
    match error {
        DecodeError::Storage(StorageError::Invariant { kind: "Head", reason: "whitespace" }) => {}
        other => panic!("expected Head whitespace invariant, got {other:?}"),
    }
}
