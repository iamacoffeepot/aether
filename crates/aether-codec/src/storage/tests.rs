//! Conformance against the derived `Storage` impls: for each fixture the
//! schema walk writes the derive's bytes and reads them back to the same
//! JSON. A walk that folds a path, hashes a variant, or picks a container's
//! element form differently from the derive fails here.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use aether_data::{
    Citations, Cites, Invariant, KindId, Schema, SchemaType, Storage, StorageData, Tag, tagged_id, with_tag,
};
use serde_json::{Value, json};

use crate::{DecodeError, EncodeError, decode_storage_schema, encode_storage_schema};

const BUDGET: usize = 10_000;

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
struct Address {
    street: String,
    zip: u32,
}

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
enum Shape {
    Empty,
    Circle(f64),
    Rect(u32, i16),
    Named { label: String, sides: u8 },
}

/// A positional element: `#[derive(Schema)]` and `repr(C)`, so its schema
/// says so.
#[derive(Debug, Clone, Copy, PartialEq, aether_data::Schema)]
#[repr(C)]
struct Point {
    x: f32,
    y: f32,
}

impl Cites for Point {
    fn cites(&self, _sink: &mut Citations) {}
}

#[derive(Debug, Clone, Copy)]
struct EmptyLabel;

impl Invariant for EmptyLabel {
    fn reason(&self) -> &'static str {
        "empty"
    }
}

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[storage(validate)]
struct Label(String);

impl Label {
    fn check(inner: &str) -> Result<(), EmptyLabel> {
        if inner.is_empty() {
            Err(EmptyLabel)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(name = "test.codec.storage.record")]
struct Record {
    id: u64,
    delta: i32,
    small: i8,
    ok: bool,
    name: String,
    payload: Vec<u8>,
    kind: KindId,
    home: Address,
    shape: Shape,
    note: Option<String>,
    home_again: Option<Address>,
    counts: Vec<u16>,
    places: Vec<Address>,
    shapes: Vec<Shape>,
    points: Vec<Point>,
    scores: BTreeMap<String, i64>,
    by_zip: BTreeMap<u32, Address>,
    digest: [u8; 4],
    label: Label,
}

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(name = "test.codec.storage.pair")]
struct Pair {
    a: u32,
    b: u32,
}

fn kind_id() -> KindId {
    KindId(with_tag(Tag::Kind, 0x00c0_dec5_7043))
}

fn full() -> (Record, Value) {
    let record = Record {
        id: 7,
        delta: -3,
        small: -128,
        ok: true,
        name: "ada".into(),
        payload: vec![1, 2, 3],
        kind: kind_id(),
        home: Address { street: "main".into(), zip: 9 },
        shape: Shape::Rect(4, -5),
        note: Some("hello".into()),
        home_again: Some(Address { street: "side".into(), zip: 10 }),
        counts: vec![1, 65_535],
        places: vec![Address { street: "a".into(), zip: 1 }, Address { street: "b".into(), zip: 2 }],
        shapes: vec![Shape::Empty, Shape::Circle(1.5), Shape::Named { label: "tri".into(), sides: 3 }],
        points: vec![Point { x: 0.5, y: -2.0 }],
        scores: BTreeMap::from([("x".into(), -1), ("y".into(), i64::MAX)]),
        by_zip: BTreeMap::from([
            (70_000, Address { street: "far".into(), zip: 70_000 }),
            (3, Address { street: "near".into(), zip: 3 }),
        ]),
        digest: [9, 8, 7, 6],
        label: Label("tag".into()),
    };
    let json = json!({
        "id": 7,
        "delta": -3,
        "small": -128,
        "ok": true,
        "name": "ada",
        "payload": [1, 2, 3],
        "kind": tagged_id::encode(kind_id().0).unwrap(),
        "home": {"street": "main", "zip": 9},
        "shape": {"Rect": [4, -5]},
        "note": "hello",
        "home_again": {"street": "side", "zip": 10},
        "counts": [1, 65_535],
        "places": [{"street": "a", "zip": 1}, {"street": "b", "zip": 2}],
        "shapes": ["Empty", {"Circle": 1.5}, {"Named": {"label": "tri", "sides": 3}}],
        "points": [{"x": 0.5, "y": -2.0}],
        "scores": {"x": -1, "y": i64::MAX},
        "by_zip": {"70000": {"street": "far", "zip": 70_000}, "3": {"street": "near", "zip": 3}},
        "digest": [9, 8, 7, 6],
        "label": "tag",
    });
    (record, json)
}

fn sparse() -> (Record, Value) {
    let record = Record {
        id: 0,
        delta: 0,
        small: 0,
        ok: false,
        name: String::new(),
        payload: Vec::new(),
        kind: kind_id(),
        home: Address { street: String::new(), zip: 0 },
        shape: Shape::Empty,
        note: None,
        home_again: None,
        counts: Vec::new(),
        places: Vec::new(),
        shapes: Vec::new(),
        points: Vec::new(),
        scores: BTreeMap::new(),
        by_zip: BTreeMap::new(),
        digest: [0; 4],
        label: Label("x".into()),
    };
    let json = json!({
        "id": 0,
        "delta": 0,
        "small": 0,
        "ok": false,
        "name": "",
        "payload": [],
        "kind": tagged_id::encode(kind_id().0).unwrap(),
        "home": {"street": "", "zip": 0},
        "shape": "Empty",
        "note": null,
        "home_again": null,
        "counts": [],
        "places": [],
        "shapes": [],
        "points": [],
        "scores": {},
        "by_zip": {},
        "digest": [0, 0, 0, 0],
        "label": "x",
    });
    (record, json)
}

fn derived<T: Storage>(value: T) -> Vec<u8> {
    T::encode_storage(&StorageData::from_value(value)).unwrap()
}

fn assert_conforms<T: Storage + Schema>(value: T, json: &Value) {
    let bytes = derived(value);
    assert_eq!(encode_storage_schema(json, &T::SCHEMA).unwrap(), bytes);
    assert_eq!(&decode_storage_schema(&bytes, &T::SCHEMA, BUDGET).unwrap(), json);
}

#[test]
fn full_record_matches_the_derived_encoding() {
    let (record, json) = full();
    assert_conforms(record, &json);
}

#[test]
fn sparse_record_matches_the_derived_encoding() {
    let (record, json) = sparse();
    assert_conforms(record, &json);
}

#[test]
fn every_enum_variant_matches_the_derived_encoding() {
    for (shape, json) in [
        (Shape::Empty, json!("Empty")),
        (Shape::Circle(-0.25), json!({"Circle": -0.25})),
        (Shape::Rect(0, i16::MIN), json!({"Rect": [0, i16::MIN]})),
        (Shape::Named { label: "sq".into(), sides: 4 }, json!({"Named": {"label": "sq", "sides": 4}})),
    ] {
        let (mut record, mut expected) = sparse();
        record.shape = shape;
        expected["shape"] = json;
        assert_conforms(record, &expected);
    }
}

#[test]
fn a_schema_decoded_from_data_encodes_the_same_bytes() {
    // A schema that arrives as data holds owned cells, which the const
    // hashes refuse; the walk must hash them to the same tags.
    let owned: SchemaType = serde_json::from_value(serde_json::to_value(&Record::SCHEMA).unwrap()).unwrap();
    let (record, json) = full();
    let bytes = derived(record);
    assert_eq!(encode_storage_schema(&json, &owned).unwrap(), bytes);
    assert_eq!(decode_storage_schema(&bytes, &owned, BUDGET).unwrap(), json);
}

#[test]
fn an_absent_option_field_encodes_as_none() {
    let (record, mut json) = sparse();
    json.as_object_mut().unwrap().remove("note");
    assert_eq!(encode_storage_schema(&json, &Record::SCHEMA).unwrap(), derived(record));
}

#[test]
fn a_missing_required_field_is_refused() {
    let (_, mut json) = full();
    json.as_object_mut().unwrap().remove("id");
    assert!(
        matches!(encode_storage_schema(&json, &Record::SCHEMA), Err(EncodeError::MissingField(path)) if path == "$.id")
    );

    let narrow = derived(Pair { a: 1, b: 2 });
    let wider = json!({"a": 0, "b": 0, "c": 0});
    let schema = widen_pair_schema();
    assert!(encode_storage_schema(&wider, &schema).is_ok());
    assert!(matches!(
        decode_storage_schema(&narrow, &schema, BUDGET),
        Err(DecodeError::MissingRecord { path }) if path == "$.c"
    ));
}

#[test]
fn a_wrong_typed_leaf_is_refused() {
    let (_, mut json) = full();
    json["home"]["zip"] = json!("nine");
    assert!(matches!(encode_storage_schema(&json, &Record::SCHEMA), Err(EncodeError::TypeMismatch { .. })));
}

#[test]
fn a_record_the_schema_does_not_bind_is_refused() {
    let bytes = derived(Pair { a: 1, b: 2 });
    let schema: SchemaType = serde_json::from_value(serde_json::to_value(&Pair::SCHEMA).unwrap()).unwrap();
    let SchemaType::Struct { fields, repr_c } = schema else {
        unreachable!("Pair is a struct")
    };
    let only_a = SchemaType::Struct { fields: fields.iter().take(1).cloned().collect::<Vec<_>>().into(), repr_c };
    assert!(matches!(decode_storage_schema(&bytes, &only_a, BUDGET), Err(DecodeError::UnboundRecord { .. })));
}

#[test]
fn a_truncated_record_is_refused() {
    let (record, _) = full();
    let bytes = derived(record);
    assert!(matches!(
        decode_storage_schema(&bytes[..bytes.len() - 1], &Record::SCHEMA, BUDGET),
        Err(DecodeError::Records { .. })
    ));
}

#[test]
fn a_value_over_the_budget_is_refused() {
    let (record, _) = full();
    assert!(matches!(
        decode_storage_schema(&derived(record), &Record::SCHEMA, 10),
        Err(DecodeError::ValueBudgetExceeded { budget: 10, .. })
    ));
}

#[test]
fn a_schema_nested_past_the_depth_cap_is_refused() {
    let mut schema = SchemaType::Scalar(aether_data::Primitive::U8);
    for _ in 0..=crate::MAX_SCHEMA_DEPTH {
        schema = SchemaType::Vec(aether_data::SchemaCell::owned(schema));
    }
    assert!(matches!(encode_storage_schema(&json!([]), &schema), Err(EncodeError::UnsupportedSchema(_))));
    assert!(matches!(decode_storage_schema(&[], &schema, BUDGET), Err(DecodeError::UnsupportedSchema(_))));
}

/// `Pair`'s schema with a third required `u32` field `c`.
fn widen_pair_schema() -> SchemaType {
    let schema: SchemaType = serde_json::from_value(serde_json::to_value(&Pair::SCHEMA).unwrap()).unwrap();
    let SchemaType::Struct { fields, repr_c } = schema else {
        unreachable!("Pair is a struct")
    };
    let mut fields = fields.into_owned();
    let mut extra = fields[0].clone();
    extra.name = "c".into();
    fields.push(extra);
    SchemaType::Struct { fields: fields.into(), repr_c }
}
