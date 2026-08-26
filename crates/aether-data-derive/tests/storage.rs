//! Derive-driven storage tests (ADR-0059). Version skew is one build:
//! encode with one type, decode with another that shares the kind name.

#![allow(clippy::unwrap_used)]

use aether_data::canonical::kind_id_from_parts;
use aether_data::hash::storage_kind_id_from_name;
use aether_data::{Kind, Schema, Storage};

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/rejects_storage_repr_c.rs");
    t.compile_fail("tests/ui/rejects_storage_reserved_prefix.rs");
    t.compile_fail("tests/ui/rejects_storage_leaf_collision.rs");
    t.compile_fail("tests/ui/rejects_storage_alias_collision.rs");
}

#[derive(Debug, PartialEq, aether_data::Schema, aether_data::Storage)]
#[kind(name = "persist.address")]
struct Address {
    street: String,
    city: String,
}

#[derive(Debug, PartialEq, aether_data::Schema, aether_data::Storage)]
#[kind(name = "persist.record")]
struct RecordV1 {
    id: u64,
    note: Option<String>,
}

#[derive(Debug, PartialEq, aether_data::Schema, aether_data::Storage)]
#[kind(name = "persist.record")]
struct RecordV2 {
    id: u64,
    note: Option<String>,
    extra: Option<u32>,
    tags: Vec<String>,
    addr: Address,
}

#[derive(Debug, PartialEq, aether_data::Schema, aether_data::Storage)]
#[kind(name = "persist.record")]
struct RecordRenamed {
    id: u64,
    #[storage(was = "note")]
    remark: Option<String>,
}

#[test]
fn storage_id_is_stable_across_schema_edits() {
    // Tripwire: storage Kind::ID is storage_kind_id_from_name, not the
    // canonical-schema hash. Adding fields must not re-key persisted rows.
    assert_eq!(RecordV1::ID, RecordV2::ID);
    assert_eq!(RecordV1::ID, storage_kind_id_from_name("persist.record"));
    assert_ne!(RecordV1::ID.0, kind_id_from_parts("persist.record", &RecordV1::SCHEMA));
    assert_ne!(RecordV1::ID.0, kind_id_from_parts("persist.record", &RecordV2::SCHEMA));
}

#[test]
fn older_reader_reencode_matches_newer_writer() {
    let newer = RecordV2 {
        id: 7,
        note: Some(String::from("hi")),
        extra: Some(3),
        tags: vec![String::from("a")],
        addr: Address { street: String::from("oak"), city: String::from("bend") },
    };
    let produced = RecordV2::encode_storage(&aether_data::StorageData::from_value(newer)).unwrap();
    let older = RecordV1::decode_storage(&produced).unwrap();
    assert_eq!(older.value.id, 7);
    assert_eq!(older.value.note.as_deref(), Some("hi"));
    assert!(!older.unknown_fields.is_empty());
    let reencoded = RecordV1::encode_storage(&older).unwrap();
    assert_eq!(reencoded, produced);
}

#[test]
fn decode_from_bytes_is_a_strict_miss() {
    let value = RecordV1 { id: 1, note: None };
    let bytes = RecordV1::encode_storage(&aether_data::StorageData::from_value(value)).unwrap();
    assert!(RecordV1::decode_from_bytes(&bytes).is_none());
}

#[test]
#[should_panic(expected = "handle indirection")]
fn encode_into_bytes_names_storage_cause() {
    let value = RecordV1 { id: 1, note: None };
    let _ = value.encode_into_bytes();
}

#[test]
fn read_alias_reads_old_name_and_writes_new() {
    let old = RecordV1 { id: 4, note: Some(String::from("keep")) };
    let old_bytes = RecordV1::encode_storage(&aether_data::StorageData::from_value(old)).unwrap();
    let renamed = RecordRenamed::decode_storage(&old_bytes).unwrap();
    assert_eq!(renamed.value.remark.as_deref(), Some("keep"));
    let new_bytes = RecordRenamed::encode_storage(&renamed).unwrap();
    let as_old = RecordV1::decode_storage(&new_bytes).unwrap();
    assert!(as_old.value.note.is_none());
    assert!(!as_old.unknown_fields.is_empty());
}

#[test]
fn every_declared_leaf_is_emitted() {
    let value = RecordV1 { id: 1, note: None };
    let bytes = RecordV1::encode_storage(&aether_data::StorageData::from_value(value)).unwrap();
    let decoded = RecordV1::decode_storage(&bytes).unwrap();
    assert!(decoded.get::<u64>("id").is_some());
    assert!(decoded.get::<u64>("note.__variant").is_some());
}

#[test]
fn nested_and_sequence_are_readable() {
    let value = RecordV2 {
        id: 2,
        note: None,
        extra: None,
        tags: vec![String::from("t")],
        addr: Address { street: String::from("main"), city: String::from("lake") },
    };
    let bytes = RecordV2::encode_storage(&aether_data::StorageData::from_value(value)).unwrap();
    let decoded = RecordV2::decode_storage(&bytes).unwrap();
    assert_eq!(decoded.get::<String>("addr.street").unwrap().unwrap(), "main");
    assert_eq!(decoded.value.tags, vec![String::from("t")]);
}

#[derive(Debug, aether_data::Schema, aether_data::Storage)]
#[storage(strict)]
#[kind(name = "persist.strict")]
struct StrictRecord {
    id: u64,
}

#[test]
fn strict_failure_names_the_unknown_leaf() {
    let newer = RecordV2 {
        id: 1,
        note: None,
        extra: Some(1),
        tags: Vec::new(),
        addr: Address { street: String::from("a"), city: String::from("b") },
    };
    let bytes = RecordV2::encode_storage(&aether_data::StorageData::from_value(newer)).unwrap();
    let err = StrictRecord::decode_storage(&bytes).unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("strict mode"), "{rendered}");
    assert!(rendered.contains("unknown field"), "{rendered}");
}
