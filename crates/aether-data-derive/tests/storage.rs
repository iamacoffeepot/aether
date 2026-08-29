//! Derive-driven storage tests (ADR-0059). Version skew is one build:
//! encode with one type, decode with another that shares the kind name.

#![allow(clippy::unwrap_used)]

use aether_data::canonical::kind_id_from_parts;
use aether_data::hash::storage_kind_id_from_name;
use aether_data::wire::WireEncode;
use aether_data::{Kind, Schema, Storage};

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/rejects_storage_repr_c.rs");
    t.compile_fail("tests/ui/rejects_storage_reserved_prefix.rs");
    t.compile_fail("tests/ui/rejects_storage_leaf_collision.rs");
    t.compile_fail("tests/ui/rejects_storage_alias_collision.rs");
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.address")]
struct Address {
    street: String,
    city: String,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.record")]
struct RecordV1 {
    id: u64,
    note: Option<String>,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.record")]
struct RecordV2 {
    id: u64,
    note: Option<String>,
    extra: Option<u32>,
    tags: Vec<String>,
    addr: Address,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
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

#[derive(Debug, aether_data::Storage)]
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

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.entry")]
struct EntryV1 {
    label: String,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.entry")]
struct EntryV2 {
    label: String,
    weight: Option<u32>,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.ledger")]
struct LedgerOld {
    entries: Vec<EntryV1>,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.ledger")]
struct LedgerNew {
    entries: Vec<EntryV2>,
}

#[derive(Debug, PartialEq, aether_data::Schema)]
struct Point {
    x: u32,
    y: u32,
}

#[derive(Debug, PartialEq, aether_data::Schema)]
struct PointWide {
    x: u32,
    y: u32,
    z: u32,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.plot")]
struct PlotOld {
    points: Vec<Point>,
}

#[derive(Debug, PartialEq, aether_data::Storage)]
#[kind(name = "persist.plot")]
struct PlotNew {
    points: Vec<PointWide>,
}

#[test]
fn tagged_element_drift_decodes_both_directions() {
    let new = LedgerNew {
        entries: vec![EntryV2 { label: "a".into(), weight: Some(3) }, EntryV2 { label: "b".into(), weight: None }],
    };
    let bytes = LedgerNew::encode_storage(&aether_data::StorageData::from_value(new)).unwrap();
    let old = LedgerOld::decode_storage(&bytes).unwrap();
    assert_eq!(old.value.entries, vec![EntryV1 { label: "a".into() }, EntryV1 { label: "b".into() }]);

    let bytes = LedgerOld::encode_storage(&aether_data::StorageData::from_value(LedgerOld {
        entries: vec![EntryV1 { label: "c".into() }],
    }))
    .unwrap();
    let new = LedgerNew::decode_storage(&bytes).unwrap();
    assert_eq!(new.value.entries, vec![EntryV2 { label: "c".into(), weight: None }]);
}

#[test]
fn tagged_container_rewrite_sheds_element_unknowns() {
    // Pins the documented v1 semantic: element-level unknown fields have
    // no side-channel on a plain value, so an old reader's rewrite sheds
    // a newer writer's element fields. Root-level unknowns still round-trip.
    let bytes = LedgerNew::encode_storage(&aether_data::StorageData::from_value(LedgerNew {
        entries: vec![EntryV2 { label: "a".into(), weight: Some(9) }],
    }))
    .unwrap();
    let rewritten = LedgerOld::encode_storage(&LedgerOld::decode_storage(&bytes).unwrap()).unwrap();
    let reread = LedgerNew::decode_storage(&rewritten).unwrap();
    assert_eq!(reread.value.entries, vec![EntryV2 { label: "a".into(), weight: None }]);
}

#[test]
fn positional_element_drift_is_a_named_miss() {
    // A `#[derive(Schema)]` element keeps the schema-folded container tag,
    // so element drift moves the tag and the reader refuses by name
    // instead of misreading positional bytes.
    let bytes = PlotNew::encode_storage(&aether_data::StorageData::from_value(PlotNew {
        points: vec![PointWide { x: 1, y: 2, z: 3 }],
    }))
    .unwrap();
    assert!(matches!(PlotOld::decode_storage(&bytes), Err(aether_data::StorageError::MissingRequiredField { .. })));
}

#[test]
fn positional_container_body_is_the_wire_encoding() {
    // Tripwire: the per-element positional path must reproduce the retired
    // opaque container encoding byte for byte — the record body is exactly
    // the container's ordinary wire bytes, so pre-element rows stay readable.
    let points = vec![Point { x: 1, y: 2 }, Point { x: 3, y: 4 }];
    let bytes = PlotOld::encode_storage(&aether_data::StorageData::from_value(PlotOld { points })).unwrap();
    let decoded = PlotOld::decode_storage(&bytes).unwrap();
    let (_, body) = decoded.get_raw::<Vec<Point>>("points").unwrap();
    let mut expected = Vec::new();
    WireEncode::encode(&vec![Point { x: 1, y: 2 }, Point { x: 3, y: 4 }], &mut expected).unwrap();
    assert_eq!(body, expected.as_slice());
}
