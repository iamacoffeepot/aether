//! Canonical `SchemaType` + `(name, schema)` kind serializers
//! (ADR-0032). Produces ADR-0118 aether-wire bytes at const-eval time
//! plus a runtime sibling (`canonical_kind_bytes`) that goes through
//! `SchemaShape` so the hub can re-derive ids for kinds decoded off
//! the wire.
//!
//! The selector constants here are hand-pinned (not source-order
//! inferred) so rearranging `SchemaType`/`EnumVariant` in `types.rs`
//! can't silently change the wire without showing up here too. They are
//! the serde `variant_index` of the matching `SchemaShape`/`VariantShape`
//! arm, written as a fixed `u32` little-endian selector — so the
//! const-fn output stays byte-identical to `wire::to_vec_bare(KindShape)`,
//! and the substrate/hub `SchemaShape` enum must keep the same order.

use crate::schema::{EnumVariant, Primitive, SchemaCell, SchemaType};

use super::primitives::{
    FLAG_WIDTH, U32_WIDTH, U64_WIDTH, cow_enum_variants, cow_named_fields, cow_schema_types,
    str_len, write_count, write_str, write_u32_le, write_u64_le,
};
use crate::schema::KindShape;
use crate::schema::SchemaShape;
use crate::schema::VariantShape;
use crate::wire;
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::vec::Vec;

const SCHEMA_UNIT: u8 = 0;
const SCHEMA_BOOL: u8 = 1;
const SCHEMA_SCALAR: u8 = 2;
const SCHEMA_STRING: u8 = 3;
const SCHEMA_BYTES: u8 = 4;
const SCHEMA_OPTION: u8 = 5;
const SCHEMA_VEC: u8 = 6;
const SCHEMA_ARRAY: u8 = 7;
const SCHEMA_STRUCT: u8 = 8;
const SCHEMA_ENUM: u8 = 9;
const SCHEMA_REF: u8 = 10;
const SCHEMA_MAP: u8 = 11;
const SCHEMA_TYPE_ID: u8 = 12;

const VARIANT_UNIT: u8 = 0;
const VARIANT_TUPLE: u8 = 1;
const VARIANT_STRUCT: u8 = 2;

const PRIM_U8: u8 = 0;
const PRIM_U16: u8 = 1;
const PRIM_U32: u8 = 2;
const PRIM_U64: u8 = 3;
const PRIM_I8: u8 = 4;
const PRIM_I16: u8 = 5;
const PRIM_I32: u8 = 6;
const PRIM_I64: u8 = 7;
const PRIM_F32: u8 = 8;
const PRIM_F64: u8 = 9;

/// Byte length the canonical schema encoding will take. Each `SchemaShape`
/// arm is a `u32` little-endian selector (`U32_WIDTH` bytes) followed by its
/// positional body — the wire encoding `wire::to_vec_bare(SchemaShape)`
/// produces.
#[must_use]
pub const fn canonical_len_schema(schema: &SchemaType) -> usize {
    match schema {
        SchemaType::Unit | SchemaType::Bool | SchemaType::String | SchemaType::Bytes => U32_WIDTH,
        // Selector + the inner `Primitive` as its own `u32` unit-variant index.
        SchemaType::Scalar(_) => U32_WIDTH + U32_WIDTH,
        SchemaType::Option(cell) | SchemaType::Vec(cell) | SchemaType::Ref(cell) => {
            U32_WIDTH + canonical_len_cell(cell)
        }
        SchemaType::Array { element, len: _ } => {
            U32_WIDTH + canonical_len_cell(element) + U32_WIDTH
        }
        SchemaType::Struct { fields, repr_c: _ } => {
            let slice = cow_named_fields(fields);
            let mut total = U32_WIDTH + U32_WIDTH;
            let mut i = 0;
            while i < slice.len() {
                total += canonical_len_schema(&slice[i].ty);
                i += 1;
            }
            total + FLAG_WIDTH
        }
        SchemaType::Enum { variants } => {
            let slice = cow_enum_variants(variants);
            let mut total = U32_WIDTH + U32_WIDTH;
            let mut i = 0;
            while i < slice.len() {
                total += canonical_len_variant(&slice[i]);
                i += 1;
            }
            total
        }
        SchemaType::Map { key, value } => {
            U32_WIDTH + canonical_len_cell(key) + canonical_len_cell(value)
        }
        SchemaType::TypeId(_) => U32_WIDTH + U64_WIDTH,
    }
}

const fn canonical_len_cell(cell: &SchemaCell) -> usize {
    match cell {
        SchemaCell::Static(r) => canonical_len_schema(r),
        SchemaCell::Owned(_) => {
            panic!("canonical: Owned SchemaCell not supported in const context");
        }
    }
}

const fn canonical_len_variant(variant: &EnumVariant) -> usize {
    match variant {
        // `VariantShape` selector + the `discriminant: u32` field.
        EnumVariant::Unit { .. } => U32_WIDTH + U32_WIDTH,
        EnumVariant::Tuple { fields, .. } => {
            let slice = cow_schema_types(fields);
            // selector + discriminant + the `fields: Vec` count.
            let mut total = U32_WIDTH + U32_WIDTH + U32_WIDTH;
            let mut i = 0;
            while i < slice.len() {
                total += canonical_len_schema(&slice[i]);
                i += 1;
            }
            total
        }
        EnumVariant::Struct { fields, .. } => {
            let slice = cow_named_fields(fields);
            let mut total = U32_WIDTH + U32_WIDTH + U32_WIDTH;
            let mut i = 0;
            while i < slice.len() {
                total += canonical_len_schema(&slice[i].ty);
                i += 1;
            }
            total
        }
    }
}

/// Serialize `schema` into `N` bytes of canonical aether-wire form.
/// Caller passes `N = canonical_len_schema(schema)`.
///
/// # Panics
/// Panics if `N` does not match the byte length the size pass
/// (`canonical_len_schema`) reports for `schema` — fail-fast per
/// ADR-0063: callers pair the two passes via the same `const` inputs,
/// so a mismatch is a bug in the serializer or its caller.
#[must_use]
pub const fn canonical_serialize_schema<const N: usize>(schema: &SchemaType) -> [u8; N] {
    let mut out = [0u8; N];
    let written = write_schema(schema, &mut out, 0);
    assert!(
        written == N,
        "canonical_serialize_schema: size mismatch between len pass and serialize pass"
    );
    out
}

/// Byte length for a full `(name, schema)` canonical record — matches
/// the bare aether-wire body of `KindShape { name, schema }`.
#[must_use]
pub const fn canonical_len_kind(name: &str, schema: &SchemaType) -> usize {
    str_len(name) + canonical_len_schema(schema)
}

/// Serialize `(name, schema)` into `N` bytes of a canonical aether-wire
/// record. These are the bytes that populate `aether.kinds` (one
/// record per `#[derive(Kind)]` type) and that `Kind::ID` hashes over.
///
/// # Panics
/// Panics if `N` does not match the byte length the size pass
/// (`canonical_len_kind`) reports for `(name, schema)` — fail-fast per
/// ADR-0063: callers pair the two passes via the same `const` inputs,
/// so a mismatch is a bug in the serializer or its caller.
#[must_use]
pub const fn canonical_serialize_kind<const N: usize>(name: &str, schema: &SchemaType) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_str(name, &mut out, 0);
    pos = write_schema(schema, &mut out, pos);
    assert!(
        pos == N,
        "canonical_serialize_kind: size mismatch between len pass and serialize pass"
    );
    out
}

/// Runtime sibling of `canonical_serialize_kind`. The derive folds the
/// const-generic variant at compile time for the `aether.kinds` link
/// section and `Kind::ID` emission; the substrate needs the same bytes
/// for a `KindDescriptor` it got over the wire or from a loaded
/// manifest, where the length isn't const and the `Cow` variants on
/// the input are `Owned` (const-path helpers panic on those).
///
/// Implementation goes `SchemaType → SchemaShape → wire::to_vec_bare`.
/// The canonical bytes are the bare aether-wire body of `KindShape {
/// name, schema: shape }`, and `SchemaShape` drops every nominal field
/// the const serializer also drops, so the two paths produce
/// byte-identical output. Pinned by the `canonical_kind_bytes_runtime_
/// matches_const` test below.
///
/// # Panics
/// Panics if wire encoding of the `KindShape` fails — fail-fast per
/// ADR-0063: `wire::to_vec_bare` into a growable `Vec` fails only when a
/// length exceeds the `u32` ceiling, unreachable for a `KindShape`, so a
/// failure indicates a serializer bug.
#[must_use]
pub fn canonical_kind_bytes(name: &str, schema: &SchemaType) -> Vec<u8> {
    let shape = KindShape {
        name: Cow::Owned(name.into()),
        schema: schema_to_shape(schema),
    };
    wire::to_vec_bare(&shape).expect("canonical KindShape serialization is infallible")
}

fn schema_to_shape(s: &SchemaType) -> SchemaShape {
    use crate::schema::SchemaShape;
    match s {
        SchemaType::Unit => SchemaShape::Unit,
        SchemaType::Bool => SchemaShape::Bool,
        SchemaType::Scalar(p) => SchemaShape::Scalar(*p),
        SchemaType::String => SchemaShape::String,
        SchemaType::Bytes => SchemaShape::Bytes,
        SchemaType::Option(cell) => SchemaShape::Option(Box::new(schema_to_shape(cell))),
        SchemaType::Vec(cell) => SchemaShape::Vec(Box::new(schema_to_shape(cell))),
        SchemaType::Array { element, len } => SchemaShape::Array {
            element: Box::new(schema_to_shape(element)),
            len: *len,
        },
        SchemaType::Struct { fields, repr_c } => SchemaShape::Struct {
            fields: fields.iter().map(|f| schema_to_shape(&f.ty)).collect(),
            repr_c: *repr_c,
        },
        SchemaType::Enum { variants } => SchemaShape::Enum {
            variants: variants.iter().map(variant_to_shape).collect(),
        },
        SchemaType::Ref(cell) => SchemaShape::Ref(Box::new(schema_to_shape(cell))),
        SchemaType::Map { key, value } => SchemaShape::Map {
            key: Box::new(schema_to_shape(key)),
            value: Box::new(schema_to_shape(value)),
        },
        SchemaType::TypeId(id) => SchemaShape::TypeId(*id),
    }
}

fn variant_to_shape(v: &EnumVariant) -> VariantShape {
    use crate::schema::VariantShape;
    match v {
        EnumVariant::Unit { discriminant, .. } => VariantShape::Unit {
            discriminant: *discriminant,
        },
        EnumVariant::Tuple {
            discriminant,
            fields,
            ..
        } => VariantShape::Tuple {
            discriminant: *discriminant,
            fields: fields.iter().map(schema_to_shape).collect(),
        },
        EnumVariant::Struct {
            discriminant,
            fields,
            ..
        } => VariantShape::Struct {
            discriminant: *discriminant,
            fields: fields.iter().map(|f| schema_to_shape(&f.ty)).collect(),
        },
    }
}

use crate::hash::{KIND_DOMAIN, fnv1a_64_prefixed};
use crate::tag_bits::{HASH_MASK, TAG_KIND, TAG_SHIFT};

/// Derive a `Kind::ID` from a `(name, schema)` pair at runtime. Matches
/// the `#[derive(Kind)]` compile-time emission byte-for-byte:
/// `Tag::Kind` `ORed` into the high 4 bits + the low 60 bits of
/// `fnv1a_64_prefixed(KIND_DOMAIN, &canonical_kind_bytes(name, schema))`.
/// A substrate computing `kind_id_from_parts(&desc.name, &desc.schema)`
/// after a runtime `register_kind_with_descriptor` agrees with the id
/// the component published as `<K as Kind>::ID` (ADR-0030 Phase 2 +
/// ADR-0064).
#[must_use]
pub fn kind_id_from_parts(name: &str, schema: &SchemaType) -> u64 {
    (u64::from(TAG_KIND) << TAG_SHIFT)
        | (fnv1a_64_prefixed(KIND_DOMAIN, &canonical_kind_bytes(name, schema)) & HASH_MASK)
}

/// Derive a `Kind::ID` from a decoded `KindShape`. Same hash as
/// `kind_id_from_parts` — the canonical bytes are the bare aether-wire
/// body of `KindShape`, so we wire-encode the shape directly without a
/// `SchemaShape → SchemaType` detour. Used by `kind_manifest` to key
/// labels records by id after decoding both sections off the wasm.
///
/// # Panics
/// Panics if wire encoding of `shape` fails — fail-fast per ADR-0063:
/// `wire::to_vec_bare` into a growable `Vec` fails only when a length
/// exceeds the `u32` ceiling, unreachable for a `KindShape`, so a
/// failure indicates a serializer bug.
#[must_use]
pub fn kind_id_from_shape(shape: &KindShape) -> u64 {
    let bytes = wire::to_vec_bare(shape).expect("canonical KindShape serialization is infallible");
    (u64::from(TAG_KIND) << TAG_SHIFT) | (fnv1a_64_prefixed(KIND_DOMAIN, &bytes) & HASH_MASK)
}

const fn write_schema(schema: &SchemaType, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    match schema {
        SchemaType::Unit => {
            pos = write_u32_le(SCHEMA_UNIT as u32, out, pos);
        }
        SchemaType::Bool => {
            pos = write_u32_le(SCHEMA_BOOL as u32, out, pos);
        }
        SchemaType::Scalar(p) => {
            pos = write_u32_le(SCHEMA_SCALAR as u32, out, pos);
            pos = write_u32_le(primitive_tag(*p) as u32, out, pos);
        }
        SchemaType::String => {
            pos = write_u32_le(SCHEMA_STRING as u32, out, pos);
        }
        SchemaType::Bytes => {
            pos = write_u32_le(SCHEMA_BYTES as u32, out, pos);
        }
        SchemaType::Option(cell) => {
            pos = write_u32_le(SCHEMA_OPTION as u32, out, pos);
            pos = write_cell(cell, out, pos);
        }
        SchemaType::Vec(cell) => {
            pos = write_u32_le(SCHEMA_VEC as u32, out, pos);
            pos = write_cell(cell, out, pos);
        }
        SchemaType::Array { element, len } => {
            pos = write_u32_le(SCHEMA_ARRAY as u32, out, pos);
            pos = write_cell(element, out, pos);
            pos = write_u32_le(*len, out, pos);
        }
        SchemaType::Struct { fields, repr_c } => {
            let slice = cow_named_fields(fields);
            pos = write_u32_le(SCHEMA_STRUCT as u32, out, pos);
            pos = write_count(slice.len(), out, pos);
            let mut i = 0;
            while i < slice.len() {
                pos = write_schema(&slice[i].ty, out, pos);
                i += 1;
            }
            out[pos] = if *repr_c { 1 } else { 0 };
            pos += 1;
        }
        SchemaType::Enum { variants } => {
            let slice = cow_enum_variants(variants);
            pos = write_u32_le(SCHEMA_ENUM as u32, out, pos);
            pos = write_count(slice.len(), out, pos);
            let mut i = 0;
            while i < slice.len() {
                pos = write_variant(&slice[i], out, pos);
                i += 1;
            }
        }
        SchemaType::Ref(cell) => {
            pos = write_u32_le(SCHEMA_REF as u32, out, pos);
            pos = write_cell(cell, out, pos);
        }
        SchemaType::Map { key, value } => {
            pos = write_u32_le(SCHEMA_MAP as u32, out, pos);
            pos = write_cell(key, out, pos);
            pos = write_cell(value, out, pos);
        }
        SchemaType::TypeId(id) => {
            pos = write_u32_le(SCHEMA_TYPE_ID as u32, out, pos);
            pos = write_u64_le(*id, out, pos);
        }
    }
    pos
}

const fn write_cell(cell: &SchemaCell, out: &mut [u8], cursor: usize) -> usize {
    match cell {
        SchemaCell::Static(r) => write_schema(r, out, cursor),
        SchemaCell::Owned(_) => {
            panic!("canonical: Owned SchemaCell not supported in const context");
        }
    }
}

const fn write_variant(variant: &EnumVariant, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    match variant {
        EnumVariant::Unit { discriminant, .. } => {
            pos = write_u32_le(VARIANT_UNIT as u32, out, pos);
            pos = write_u32_le(*discriminant, out, pos);
        }
        EnumVariant::Tuple {
            discriminant,
            fields,
            ..
        } => {
            let slice = cow_schema_types(fields);
            pos = write_u32_le(VARIANT_TUPLE as u32, out, pos);
            pos = write_u32_le(*discriminant, out, pos);
            pos = write_count(slice.len(), out, pos);
            let mut i = 0;
            while i < slice.len() {
                pos = write_schema(&slice[i], out, pos);
                i += 1;
            }
        }
        EnumVariant::Struct {
            discriminant,
            fields,
            ..
        } => {
            let slice = cow_named_fields(fields);
            pos = write_u32_le(VARIANT_STRUCT as u32, out, pos);
            pos = write_u32_le(*discriminant, out, pos);
            pos = write_count(slice.len(), out, pos);
            let mut i = 0;
            while i < slice.len() {
                pos = write_schema(&slice[i].ty, out, pos);
                i += 1;
            }
        }
    }
    pos
}

const fn primitive_tag(p: Primitive) -> u8 {
    match p {
        Primitive::U8 => PRIM_U8,
        Primitive::U16 => PRIM_U16,
        Primitive::U32 => PRIM_U32,
        Primitive::U64 => PRIM_U64,
        Primitive::I8 => PRIM_I8,
        Primitive::I16 => PRIM_I16,
        Primitive::I32 => PRIM_I32,
        Primitive::I64 => PRIM_I64,
        Primitive::F32 => PRIM_F32,
        Primitive::F64 => PRIM_F64,
    }
}
