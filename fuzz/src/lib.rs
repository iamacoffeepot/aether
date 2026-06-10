//! Shared schema table for the `decode_schema` fuzz target and its
//! corpus generator.
//!
//! `decode_schema` is the substrate's untrusted-input boundary: raw
//! wire bytes flow into a hand-rolled dual-path decoder (bytemuck cast
//! vs postcard). In production the `SchemaType` descriptor comes from
//! the trusted kind registry and only the bytes are attacker-shaped, so
//! the harness holds a fixed, deterministic table of schemas and lets
//! the fuzzer pick one with the input's leading byte; the remaining
//! bytes are the adversarial payload.
//!
//! Selector routing is an explicit `match` with stable arms and a
//! default arm. Appending a schema adds an arm without remapping the
//! existing selectors, so a corpus entry keeps decoding against the
//! same schema across table revisions.

use std::borrow::Cow;

use aether_data::{EnumVariant, NamedField, Primitive, SchemaCell, SchemaType};
use serde_json::{Value, json};

/// Number of schemas in the table. Selector bytes `0..TABLE_LEN` map to
/// distinct schemas; everything else falls through the default arm.
pub const TABLE_LEN: u8 = 7;

/// Map a selector byte to one of the table's fixed schemas. Stable
/// arms plus a default arm: the byte-to-schema mapping never shifts
/// when a schema is appended, so seed and accreted corpus entries stay
/// valid.
#[must_use]
pub fn schema_for(selector: u8) -> SchemaType {
    match selector {
        0 => cast_scalars(),
        1 => postcard_string_vec_option(),
        2 => multi_variant_enum(),
        3 => string_map(),
        4 => nested_option_vec_array(),
        5 => SchemaType::Bytes,
        6 => ref_cast_inner(),
        // Default arm: route every other selector to schema 0 so the
        // mapping is total and stable.
        _ => cast_scalars(),
    }
}

/// One seed per table schema: the selector byte plus a JSON value that
/// `encode_schema` accepts for that schema. The corpus generator
/// prepends the selector to each encoded frame so the fuzzer starts
/// from valid framing instead of rediscovering it from zero.
#[must_use]
pub fn seeds() -> Vec<(u8, Value)> {
    vec![
        (0, json!({ "a": 7, "b": 16_909_060, "c": -3, "d": 1.5 })),
        (
            1,
            json!({ "name": "Aether", "tags": ["alpha", "beta", "gamma"], "opt": 5 }),
        ),
        (2, json!({ "Ok": 42 })),
        (
            3,
            json!({ "x-trace": "abc123", "content-type": "application/json" }),
        ),
        (4, json!([[1, 2, 3], [4, 5, 6]])),
        (5, json!([1, 2, 3, 4, 5])),
        (6, json!({ "Handle": { "id": 7, "kind_id": 42 } })),
    ]
}

fn field(name: &'static str, ty: SchemaType) -> NamedField {
    NamedField {
        name: Cow::Borrowed(name),
        ty,
    }
}

fn scalar(name: &'static str, primitive: Primitive) -> NamedField {
    field(name, SchemaType::Scalar(primitive))
}

/// Schema 0: a `#[repr(C)]` cast struct of scalars. Exercises the
/// bytemuck-cast path: alignment padding between fields and the
/// largest-field-alignment trailing pad.
fn cast_scalars() -> SchemaType {
    SchemaType::Struct {
        fields: Cow::Owned(vec![
            scalar("a", Primitive::U8),
            scalar("b", Primitive::U32),
            scalar("c", Primitive::I16),
            scalar("d", Primitive::F32),
        ]),
        repr_c: true,
    }
}

/// Schema 1: a postcard struct mixing `String`, `Vec`, and `Option`.
/// Exercises length-prefixed strings/vecs and the option discriminant.
fn postcard_string_vec_option() -> SchemaType {
    SchemaType::Struct {
        fields: Cow::Owned(vec![
            field("name", SchemaType::String),
            field(
                "tags",
                SchemaType::Vec(SchemaCell::owned(SchemaType::String)),
            ),
            field(
                "opt",
                SchemaType::Option(SchemaCell::owned(SchemaType::Scalar(Primitive::U32))),
            ),
        ]),
        repr_c: false,
    }
}

/// Schema 2: a multi-variant enum (unit, tuple, two struct variants).
/// Exercises the discriminant varint and per-variant body walks.
fn multi_variant_enum() -> SchemaType {
    SchemaType::Enum {
        variants: Cow::Owned(vec![
            EnumVariant::Unit {
                name: Cow::Borrowed("Pending"),
                discriminant: 0,
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Ok"),
                discriminant: 1,
                fields: Cow::Owned(vec![SchemaType::Scalar(Primitive::U64)]),
            },
            EnumVariant::Struct {
                name: Cow::Borrowed("Err"),
                discriminant: 2,
                fields: Cow::Owned(vec![field("reason", SchemaType::String)]),
            },
            EnumVariant::Struct {
                name: Cow::Borrowed("Retry"),
                discriminant: 3,
                fields: Cow::Owned(vec![scalar("after", Primitive::U32)]),
            },
        ]),
    }
}

/// Schema 3: a `Map<String, String>`. Exercises the keyed-table varint
/// length prefix and the key-stringify rule.
fn string_map() -> SchemaType {
    SchemaType::Map {
        key: SchemaCell::owned(SchemaType::String),
        value: SchemaCell::owned(SchemaType::String),
    }
}

/// Schema 4: a nested `Option<Vec<[u8; 3]>>`. Exercises option-around-
/// collection-around-fixed-array recursion.
fn nested_option_vec_array() -> SchemaType {
    SchemaType::Option(SchemaCell::owned(SchemaType::Vec(SchemaCell::owned(
        SchemaType::Array {
            element: SchemaCell::owned(SchemaType::Scalar(Primitive::U8)),
            len: 3,
        },
    ))))
}

/// Schema 6: a `Ref` whose inner kind is a cast struct. Exercises the
/// ADR-0045/0100 inline-vs-handle tag plus the length-prefixed inner
/// cast image on the inline arm.
fn ref_cast_inner() -> SchemaType {
    SchemaType::Ref(SchemaCell::owned(SchemaType::Struct {
        fields: Cow::Owned(vec![scalar("code", Primitive::U32)]),
        repr_c: true,
    }))
}
