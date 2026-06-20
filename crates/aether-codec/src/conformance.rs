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

use aether_data::wire;
use aether_data::{Kind, KindId, Primitive, Ref, SchemaCell, SchemaType};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::test_fixtures::{named, scalar, structured_struct};
use crate::{decode_schema, encode_schema};

/// The conformance law for one `(value, schema, json)` fixture: the
/// serde adapter and the schema walker emit identical versioned bytes,
/// and each side decodes the other's bytes back to the canonical form.
fn check<T>(value: &T, schema: &SchemaType, json: &Value)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let adapter = wire::to_vec(value).expect("wire adapter encode");
    let walker = encode_schema(json, schema).expect("schema walker encode");
    assert_eq!(
        adapter, walker,
        "adapter vs walker encode bytes diverge for {json}"
    );

    let decoded_json = decode_schema(&adapter, schema).expect("walker decode of adapter bytes");
    assert_eq!(
        &decoded_json, json,
        "walker decode of adapter bytes diverges for {json}"
    );

    let decoded_value: T = wire::from_bytes(&walker).expect("adapter decode of walker bytes");
    assert_eq!(
        &decoded_value, value,
        "adapter decode of walker bytes diverges"
    );
}

#[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
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

#[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
struct Inner {
    seq: u32,
}

#[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
struct Collections {
    tags: Vec<String>,
    maybe_some: Option<u64>,
    maybe_none: Option<u64>,
    triple: [u32; 3],
    blob: Vec<u8>,
    nested: Inner,
}

fn collections_schema() -> SchemaType {
    structured_struct(vec![
        named(
            "tags",
            SchemaType::Vec(SchemaCell::owned(SchemaType::String)),
        ),
        named(
            "maybe_some",
            SchemaType::Option(SchemaCell::owned(SchemaType::Scalar(Primitive::U64))),
        ),
        named(
            "maybe_none",
            SchemaType::Option(SchemaCell::owned(SchemaType::Scalar(Primitive::U64))),
        ),
        named(
            "triple",
            SchemaType::Array {
                element: SchemaCell::owned(SchemaType::Scalar(Primitive::U32)),
                len: 3,
            },
        ),
        named("blob", SchemaType::Bytes),
        named(
            "nested",
            structured_struct(vec![scalar("seq", Primitive::U32)]),
        ),
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

#[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
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
            EnumVariant::Unit {
                name: "Pending".into(),
                discriminant: 0,
            },
            EnumVariant::Tuple {
                name: "Ok".into(),
                discriminant: 1,
                fields: vec![SchemaType::Scalar(Primitive::U64)].into(),
            },
            EnumVariant::Tuple {
                name: "Pair".into(),
                discriminant: 2,
                fields: vec![
                    SchemaType::Scalar(Primitive::U32),
                    SchemaType::Scalar(Primitive::I16),
                ]
                .into(),
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
    check(
        &Sum::Ok(0x0102_0304),
        &sum_schema(),
        &json!({ "Ok": 0x0102_0304u64 }),
    );
    check(
        &Sum::Pair(7, -3),
        &sum_schema(),
        &json!({ "Pair": [7u32, -3i16] }),
    );
    check(
        &Sum::Err {
            reason: "boom".into(),
        },
        &sum_schema(),
        &json!({ "Err": { "reason": "boom" } }),
    );
}

#[test]
fn map_keys_conform_in_encoded_byte_order() {
    use std::collections::BTreeMap;
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

// A wire-shaped inner kind for the `Ref` fixtures. Hand-rolled (not
// derived) so the conformance module stays free of the `Kind` derive's
// inventory machinery; its codec is `wire`, matching the schema-walker's
// non-cast path.
#[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
struct Leaf {
    code: u32,
    tag: String,
}

impl Kind for Leaf {
    const NAME: &'static str = "conformance.leaf";
    const ID: KindId = KindId(0xC0FF_EE00_0000_0001);

    fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
        wire::from_bytes(bytes).ok()
    }

    fn encode_into_bytes(&self) -> Vec<u8> {
        wire::to_vec(self).expect("wire encode")
    }
}

fn leaf_schema() -> SchemaType {
    structured_struct(vec![
        scalar("code", Primitive::U32),
        named("tag", SchemaType::String),
    ])
}

#[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
struct RefHolder {
    held: Ref<Leaf>,
    seq: u32,
}

fn ref_holder_schema() -> SchemaType {
    structured_struct(vec![
        named("held", SchemaType::Ref(SchemaCell::owned(leaf_schema()))),
        scalar("seq", Primitive::U32),
    ])
}

#[test]
fn ref_inline_conforms_with_nested_versioned_image() {
    let value = RefHolder {
        held: Ref::Inline(Leaf {
            code: 0x0102_0304,
            tag: "leaf".into(),
        }),
        seq: 9,
    };
    let json = json!({
        "held": { "Inline": { "code": 0x0102_0304u32, "tag": "leaf" } },
        "seq": 9u32,
    });
    check(&value, &ref_holder_schema(), &json);
}

#[test]
fn ref_handle_conforms() {
    let value = RefHolder {
        held: Ref::handle(0x00CA_FE00),
        seq: 11,
    };
    let json = json!({
        "held": { "Handle": { "id": 0x00CA_FE00u64, "kind_id": Leaf::ID.0 } },
        "seq": 11u32,
    });
    check(&value, &ref_holder_schema(), &json);
}
