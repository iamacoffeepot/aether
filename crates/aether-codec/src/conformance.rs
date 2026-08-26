//! Adapter-vs-walker conformance for the `aether_data::wire` format
//! (ADR-0118 step 2A). Two independent encoders write the same byte
//! layout: the serde adapter (`aether_data::wire::{to_vec, from_bytes}`,
//! driven by a Rust type) and this crate's schema-driven JSON walker
//! (`encode_schema` / `decode_schema`, driven by a `SchemaType`). They
//! must agree byte-for-byte for the same logical value, or a kind sent
//! over the hub's JSON path would decode differently from one sent
//! through the guest's typed path.
//!
//! The deferred cross-check from #1980: for every fixture, assert
//!
//! ```text
//! wire::to_vec(value)              == encode_schema(json, schema)
//! decode_schema(wire::to_vec(v), s) == json
//! wire::from_bytes(encode_schema(j, s)) == value
//! ```
//!
//! all three over versioned images on both sides — the serde adapter
//! and both halves of the schema walker. The handle-store walker (the
//! third reader of this layout) is pinned to the same bytes by its own
//! round-trip tests in `aether-substrate`.

#![cfg(test)]

use core::fmt::Debug;
use std::collections::BTreeMap;

use aether_data::wire::{self, WireDecode, WireEncode, decode_from_slice, encode_to_vec};
use aether_data::{Primitive, SchemaCell, SchemaType};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::test_fixtures::{named, scalar, structured_struct};
use crate::{decode_schema, encode_schema};

/// The conformance law for one `(value, schema, json)` fixture: the
/// owned codec, the serde adapter, and the schema walker emit identical
/// bytes, and each decoder accepts the others' output.
fn check<T>(value: &T, schema: &SchemaType, json: &Value)
where
    T: Serialize + DeserializeOwned + WireEncode + for<'de> WireDecode<'de> + PartialEq + Debug,
{
    let adapter = wire::to_vec(value).expect("wire adapter encode");
    let derived = encode_to_vec(value).expect("owned encode");
    let walker = encode_schema(json, schema).expect("schema walker encode");
    assert_eq!(adapter, walker, "adapter vs walker encode bytes diverge for {json}");
    assert_eq!(derived, adapter, "owned codec vs serde adapter encode bytes diverge for {json}");

    let decoded_json = decode_schema(&adapter, schema).expect("walker decode of adapter bytes");
    assert_eq!(&decoded_json, json, "walker decode of adapter bytes diverges for {json}");

    let decoded_value: T = wire::from_bytes(&walker).expect("adapter decode of walker bytes");
    assert_eq!(&decoded_value, value, "adapter decode of walker bytes diverges");

    let from_derived: T = decode_from_slice(&walker).expect("owned decode of walker bytes");
    assert_eq!(&from_derived, value, "owned decode of walker bytes diverges");
}

#[derive(Serialize, serde::Deserialize, aether_data::Schema, PartialEq, Debug)]
struct Scalars {
    a: u8,
    b: u16,
    c: u32,
    d: u64,
    e: i8,
    f: i16,
    g: i32,
    h: i64,
    x: f32,
    y: f64,
    flag: bool,
    label: String,
}

fn scalars_schema() -> SchemaType {
    structured_struct(vec![
        scalar("a", Primitive::U8),
        scalar("b", Primitive::U16),
        scalar("c", Primitive::U32),
        scalar("d", Primitive::U64),
        scalar("e", Primitive::I8),
        scalar("f", Primitive::I16),
        scalar("g", Primitive::I32),
        scalar("h", Primitive::I64),
        scalar("x", Primitive::F32),
        scalar("y", Primitive::F64),
        named("flag", SchemaType::Bool),
        named("label", SchemaType::String),
    ])
}

#[test]
fn scalars_conform() {
    let value = Scalars {
        a: 1,
        b: 0x0102,
        c: 0x0102_0304,
        d: 0x0102_0304_0506_0708,
        e: -1,
        f: -300,
        g: -70_000,
        h: -5_000_000_000,
        x: 1.5,
        y: -2.25,
        flag: true,
        label: "héllo".into(),
    };
    let json = json!({
        "a": 1u8,
        "b": 0x0102u16,
        "c": 0x0102_0304u32,
        "d": 0x0102_0304_0506_0708u64,
        "e": -1i8,
        "f": -300i16,
        "g": -70_000i32,
        "h": -5_000_000_000i64,
        "x": 1.5,
        "y": -2.25,
        "flag": true,
        "label": "héllo",
    });
    check(&value, &scalars_schema(), &json);
}

#[derive(Serialize, serde::Deserialize, aether_data::Schema, PartialEq, Debug)]
struct Inner {
    seq: u32,
}

#[derive(Serialize, serde::Deserialize, aether_data::Schema, PartialEq, Debug)]
struct Collections {
    tags: Vec<String>,
    maybe_some: Option<u64>,
    maybe_none: Option<u64>,
    triple: [u32; 3],
    #[serde(with = "aether_data::bytes")]
    blob: Vec<u8>,
    nested: Inner,
}

fn collections_schema() -> SchemaType {
    structured_struct(vec![
        named("tags", SchemaType::Vec(SchemaCell::owned(SchemaType::String))),
        named("maybe_some", SchemaType::Option(SchemaCell::owned(SchemaType::Scalar(Primitive::U64)))),
        named("maybe_none", SchemaType::Option(SchemaCell::owned(SchemaType::Scalar(Primitive::U64)))),
        named("triple", SchemaType::Array { element: SchemaCell::owned(SchemaType::Scalar(Primitive::U32)), len: 3 }),
        named("blob", SchemaType::Bytes),
        named("nested", structured_struct(vec![scalar("seq", Primitive::U32)])),
    ])
}

#[test]
fn collections_conform() {
    let value = Collections {
        tags: vec!["alpha".into(), "beta".into()],
        maybe_some: Some(0x0102_0304_0506_0708),
        maybe_none: None,
        triple: [1, 0x0001_0000, 0xFFFF_FFFF],
        blob: vec![0, 1, 2, 200, 255],
        nested: Inner { seq: 0xDEAD_BEEF },
    };
    let json = json!({
        "tags": ["alpha", "beta"],
        "maybe_some": 0x0102_0304_0506_0708u64,
        "maybe_none": null,
        "triple": [1u32, 0x0001_0000u32, 0xFFFF_FFFFu32],
        "blob": [0, 1, 2, 200, 255],
        "nested": { "seq": 0xDEAD_BEEFu32 },
    });
    check(&value, &collections_schema(), &json);
}

#[derive(Serialize, serde::Deserialize, aether_data::Schema, PartialEq, Debug)]
enum Sum {
    Pending,
    Ok(u64),
    Pair(u32, i16),
    Err { reason: String },
}

fn sum_schema() -> SchemaType {
    use aether_data::EnumVariant;
    SchemaType::Enum {
        variants: vec![
            EnumVariant::Unit { name: "Pending".into(), discriminant: 0 },
            EnumVariant::Tuple {
                name: "Ok".into(),
                discriminant: 1,
                fields: vec![SchemaType::Scalar(Primitive::U64)].into(),
            },
            EnumVariant::Tuple {
                name: "Pair".into(),
                discriminant: 2,
                fields: vec![SchemaType::Scalar(Primitive::U32), SchemaType::Scalar(Primitive::I16)].into(),
            },
            EnumVariant::Struct {
                name: "Err".into(),
                discriminant: 3,
                fields: vec![named("reason", SchemaType::String)].into(),
            },
        ]
        .into(),
    }
}

#[test]
fn enum_variants_conform() {
    check(&Sum::Pending, &sum_schema(), &json!("Pending"));
    check(&Sum::Ok(0x0102_0304), &sum_schema(), &json!({ "Ok": 0x0102_0304u64 }));
    check(&Sum::Pair(7, -3), &sum_schema(), &json!({ "Pair": [7u32, -3i16] }));
    check(&Sum::Err { reason: "boom".into() }, &sum_schema(), &json!({ "Err": { "reason": "boom" } }));
}

#[test]
fn map_keys_conform_in_encoded_byte_order() {
    // Keys 1 and 256 sort numerically as 1 < 256 but in little-endian
    // u32 bytes as 256 < 1 — the multi-byte key case the encoded-byte
    // map ordering must reproduce.
    let mut value: BTreeMap<u32, String> = BTreeMap::new();
    value.insert(1, "one".into());
    value.insert(256, "two-fifty-six".into());
    let schema = SchemaType::Map {
        key: SchemaCell::owned(SchemaType::Scalar(Primitive::U32)),
        value: SchemaCell::owned(SchemaType::String),
    };
    let json = json!({ "1": "one", "256": "two-fifty-six" });
    check(&value, &schema, &json);
}

#[test]
fn fixture_kinds_corpus_covers_every_schema_arm() {
    use aether_data::Schema;
    use aether_test_fixtures_kinds::wire_corpus::{
        CorpusCast, CorpusCollections, CorpusMaps, CorpusNested, CorpusScalars, CorpusSum, CorpusUnit,
    };

    check(&CorpusUnit, &CorpusUnit::SCHEMA, &json!(null));

    let scalars = CorpusScalars {
        u8_field: 1,
        u16_field: 0x0102,
        u32_field: 0x0102_0304,
        u64_field: 0x0102_0304_0506_0708,
        i8_field: -1,
        i16_field: -300,
        i32_field: -70_000,
        i64_field: -5_000_000_000,
        f32_field: 1.5,
        f64_field: -2.25,
        flag: true,
        label: "héllo".into(),
    };
    check(
        &scalars,
        &CorpusScalars::SCHEMA,
        &json!({
            "u8_field": 1u8,
            "u16_field": 0x0102u16,
            "u32_field": 0x0102_0304u32,
            "u64_field": 0x0102_0304_0506_0708u64,
            "i8_field": -1i8,
            "i16_field": -300i16,
            "i32_field": -70_000i32,
            "i64_field": -5_000_000_000i64,
            "f32_field": 1.5,
            "f64_field": -2.25,
            "flag": true,
            "label": "héllo",
        }),
    );

    let collections = CorpusCollections {
        tags: vec!["alpha".into(), "beta".into()],
        maybe_some: Some(9),
        maybe_none: None,
        triple: [1, 2, 3],
        blob: vec![0, 1, 255],
        empty_vec: vec![],
        empty_string: String::new(),
    };
    check(
        &collections,
        &CorpusCollections::SCHEMA,
        &json!({
            "tags": ["alpha", "beta"],
            "maybe_some": 9u64,
            "maybe_none": null,
            "triple": [1u32, 2u32, 3u32],
            "blob": [0, 1, 255],
            "empty_vec": [],
            "empty_string": "",
        }),
    );

    check(&CorpusSum::Pending, &CorpusSum::SCHEMA, &json!("Pending"));
    check(&CorpusSum::Ok(7), &CorpusSum::SCHEMA, &json!({ "Ok": 7u64 }));
    check(&CorpusSum::Pair(1, -2), &CorpusSum::SCHEMA, &json!({ "Pair": [1u32, -2i16] }));
    check(&CorpusSum::Err { reason: "nope".into() }, &CorpusSum::SCHEMA, &json!({ "Err": { "reason": "nope" } }));

    let nested = CorpusNested { items: vec![None, Some(CorpusSum::Pending), Some(CorpusSum::Ok(1))] };
    check(&nested, &CorpusNested::SCHEMA, &json!({ "items": [null, "Pending", { "Ok": 1u64 }] }));

    let mut by_name = BTreeMap::new();
    by_name.insert("b".into(), 1u8);
    by_name.insert("aa".into(), 2u8);
    let mut by_u32 = BTreeMap::new();
    by_u32.insert(1u32, "one".into());
    by_u32.insert(256u32, "two-fifty-six".into());
    check(
        &CorpusMaps { by_name, by_u32 },
        &CorpusMaps::SCHEMA,
        &json!({
            "by_name": { "aa": 2, "b": 1 },
            "by_u32": { "1": "one", "256": "two-fifty-six" },
        }),
    );

    check(&CorpusCast { x: 1.5, y: -2.25 }, &CorpusCast::SCHEMA, &json!({ "x": 1.5, "y": -2.25 }));
}
