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
use std::collections::BTreeMap;

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

/// A multi-variant enum for the wire-format fuzz targets. Every arm
/// exercises a different `variant_index` selector path in the `de` adapter.
#[derive(arbitrary::Arbitrary, serde::Serialize, serde::Deserialize, Debug, Clone)]
pub enum WireVariant {
    /// Unit variant — body is empty.
    Unit,
    /// Tuple variant — body is one `u64`.
    Tuple(u64),
    /// Struct variant — body is one named `u32` field.
    Struct { code: u32 },
}

/// A nested struct carried inside `WireValue` to exercise positional field
/// encoding without field names or counts.
#[derive(arbitrary::Arbitrary, serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct WireNested {
    pub x: i32,
    pub y: i32,
}

/// Corpus type for the `wire_roundtrip` fuzz target.
///
/// Roundtrip is a symmetry check over *valid* values, so one composite
/// type that spans every branch is the right shape: a single `Arbitrary`
/// value exercises all of them at once, and there is no positional gating
/// to worry about (the value is generated, not decoded from bytes).
///
/// Panic-freedom on *malformed* bytes is a different concern with a
/// different failure mode — the decoder consumes bytes positionally, so a
/// risky path buried behind earlier fields is reached only after the fuzzer
/// learns a prefix past them. That surface is covered by the focused
/// `wire_decode_{str,seq,enum}` targets below, each decoding into a small
/// type that puts its risk path at byte zero.
///
/// Covers every encoding branch documented in `aether_data::wire`:
///
/// - Fixed-width scalars (`u8`, `u16`, `u32`, `u64`, `i8`, `i16`, `i32`,
///   `i64`, `f32`, `f64`).
/// - `bool` and option-presence (one byte, `0` / `1`).
/// - `String` (length-prefixed UTF-8).
/// - `serde_bytes::ByteBuf` — hits the `serialize_bytes` / length-prefixed
///   byte-sequence path rather than the per-element `Vec<u8>` path.
/// - `Vec<u32>` — length-prefixed element sequence.
/// - `BTreeMap<String, u32>` — length-prefixed map in ascending key order.
/// - `WireVariant` — multi-variant enum, `variant_index` selector.
/// - `WireNested` — nested struct, positional fields.
/// - A two-element tuple `(u16, u64)`.
/// - A fixed-size byte array `[u8; 4]`.
///
/// `PartialEq` is deliberately omitted: the roundtrip assertion compares
/// re-encoded bytes rather than values, which is `NaN`-safe for `f32`/`f64`
/// (bit-faithful format, `NaN != NaN` but identical bit patterns re-encode
/// identically).
#[derive(arbitrary::Arbitrary, serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct WireValue {
    pub u8_field: u8,
    pub u16_field: u16,
    pub u32_field: u32,
    pub u64_field: u64,
    pub i8_field: i8,
    pub i16_field: i16,
    pub i32_field: i32,
    pub i64_field: i64,
    pub f32_field: f32,
    pub f64_field: f64,
    pub bool_field: bool,
    pub opt_field: Option<u32>,
    pub string_field: String,
    #[serde(with = "serde_bytes")]
    pub bytes_field: Vec<u8>,
    pub vec_field: Vec<u32>,
    pub map_field: BTreeMap<String, u32>,
    pub variant_field: WireVariant,
    pub nested_field: WireNested,
    pub tuple_field: (u16, u64),
    pub array_field: [u8; 4],
}

/// Decode-target corpus for the `wire_decode_str` target: the string
/// length-prefix and UTF-8 paths, ungated.
///
/// `String` decode reads a length prefix and then validates the bytes as
/// UTF-8 — the two ways a string decode can fault (`Length` past the
/// buffer, `Utf8` on bad bytes). Both fields are strings so the first one
/// faults on byte zero; the `Option<String>` adds the option discriminant
/// (`0` / `1`) wrapped around a string. Only `Deserialize` is needed — the
/// target decodes arbitrary bytes and discards the result.
#[derive(serde::Deserialize)]
pub struct WireStrings {
    pub a: String,
    pub b: String,
    pub c: Option<String>,
}

/// Decode-target corpus for the `wire_decode_seq` target: the
/// collection length-prefix paths, ungated.
///
/// Each field is a length-prefixed collection — a per-element `Vec`, a
/// `serialize_bytes` byte sequence, and an ascending-key map — so a
/// malformed length (claiming more elements than the buffer holds) must
/// return `Err` rather than over-read or pre-allocate unboundedly. The
/// `Vec<u32>` leads so its length read faults at byte zero.
#[derive(serde::Deserialize)]
pub struct WireSeqs {
    pub list: Vec<u32>,
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
    pub map: BTreeMap<String, u32>,
}

/// Decode-target corpus for the `wire_decode_enum` target: the
/// discriminant and scalar-validation paths, ungated.
///
/// The leading `WireVariant` exercises the `variant_index` selector (an
/// out-of-range discriminant must `Err`, not index past the variant
/// table); `bool` exercises the `0` / `1` validation, `char` the
/// `InvalidChar` path, and the trailing `u64` a fixed-width read that
/// faults on truncation. `WireVariant` already derives `Deserialize`.
#[derive(serde::Deserialize)]
pub struct WireDiscriminants {
    pub variant: WireVariant,
    pub flag: bool,
    pub ch: char,
    pub scalar: u64,
}
