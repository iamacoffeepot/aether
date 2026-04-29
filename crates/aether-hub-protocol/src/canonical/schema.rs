//! Canonical `SchemaType` + `(name, schema)` kind serializers
//! (ADR-0032). Produces postcard-compatible bytes at const-eval time
//! plus a runtime sibling (`canonical_kind_bytes`) that goes through
//! `SchemaShape` so the hub can re-derive ids for kinds decoded off
//! the wire.
//!
//! The wire-tag constants here are hand-pinned (not source-order
//! inferred) so rearranging `SchemaType`/`EnumVariant` in `types.rs`
//! can't silently change the wire without showing up here too. The
//! substrate/hub `SchemaShape` enum must keep the same order.

use crate::types::{EnumVariant, Primitive, SchemaCell, SchemaType};

use super::primitives::{
    cow_enum_variants, cow_named_fields, cow_schema_types, str_len, varint_u32_len,
    varint_usize_len, write_str, write_varint_u32, write_varint_usize,
};

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

/// Byte length the canonical schema encoding will take.
pub const fn canonical_len_schema(schema: &SchemaType) -> usize {
    match schema {
        SchemaType::Unit => 1,
        SchemaType::Bool => 1,
        SchemaType::Scalar(_) => 1 + 1,
        SchemaType::String => 1,
        SchemaType::Bytes => 1,
        SchemaType::Option(cell) => 1 + canonical_len_cell(cell),
        SchemaType::Vec(cell) => 1 + canonical_len_cell(cell),
        SchemaType::Array { element, len } => {
            1 + canonical_len_cell(element) + varint_u32_len(*len)
        }
        SchemaType::Struct { fields, repr_c: _ } => {
            let slice = cow_named_fields(fields);
            let mut total = 1 + varint_usize_len(slice.len());
            let mut i = 0;
            while i < slice.len() {
                total += canonical_len_schema(&slice[i].ty);
                i += 1;
            }
            total + 1
        }
        SchemaType::Enum { variants } => {
            let slice = cow_enum_variants(variants);
            let mut total = 1 + varint_usize_len(slice.len());
            let mut i = 0;
            while i < slice.len() {
                total += canonical_len_variant(&slice[i]);
                i += 1;
            }
            total
        }
        SchemaType::Ref(cell) => 1 + canonical_len_cell(cell),
        SchemaType::Map { key, value } => 1 + canonical_len_cell(key) + canonical_len_cell(value),
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
        EnumVariant::Unit { discriminant, .. } => 1 + varint_u32_len(*discriminant),
        EnumVariant::Tuple {
            discriminant,
            fields,
            ..
        } => {
            let slice = cow_schema_types(fields);
            let mut total = 1 + varint_u32_len(*discriminant) + varint_usize_len(slice.len());
            let mut i = 0;
            while i < slice.len() {
                total += canonical_len_schema(&slice[i]);
                i += 1;
            }
            total
        }
        EnumVariant::Struct {
            discriminant,
            fields,
            ..
        } => {
            let slice = cow_named_fields(fields);
            let mut total = 1 + varint_u32_len(*discriminant) + varint_usize_len(slice.len());
            let mut i = 0;
            while i < slice.len() {
                total += canonical_len_schema(&slice[i].ty);
                i += 1;
            }
            total
        }
    }
}

/// Serialize `schema` into `N` bytes of canonical postcard form.
/// Caller passes `N = canonical_len_schema(schema)`.
pub const fn canonical_serialize_schema<const N: usize>(schema: &SchemaType) -> [u8; N] {
    let mut out = [0u8; N];
    let written = write_schema(schema, &mut out, 0);
    if written != N {
        panic!("canonical_serialize_schema: size mismatch between len pass and serialize pass");
    }
    out
}

/// Byte length for a full `(name, schema)` canonical record —
/// matches `postcard(KindShape { name, schema })`.
pub const fn canonical_len_kind(name: &str, schema: &SchemaType) -> usize {
    str_len(name) + canonical_len_schema(schema)
}

/// Serialize `(name, schema)` into `N` bytes of a canonical postcard
/// record. These are the bytes that populate `aether.kinds` (one
/// record per `#[derive(Kind)]` type) and that `Kind::ID` hashes over.
pub const fn canonical_serialize_kind<const N: usize>(name: &str, schema: &SchemaType) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_str(name, &mut out, 0);
    pos = write_schema(schema, &mut out, pos);
    if pos != N {
        panic!("canonical_serialize_kind: size mismatch between len pass and serialize pass");
    }
    out
}

/// Runtime sibling of `canonical_serialize_kind`. The derive folds the
/// const-generic variant at compile time for the `aether.kinds` link
/// section and `Kind::ID` emission; the substrate needs the same bytes
/// for a `KindDescriptor` it got over the wire or from a loaded
/// manifest, where the length isn't const and the `Cow` variants on
/// the input are `Owned` (const-path helpers panic on those).
///
/// Implementation goes `SchemaType → SchemaShape → postcard`. The
/// canonical bytes format is defined as `postcard(KindShape { name,
/// schema: shape })`, and `SchemaShape` drops every nominal field the
/// const serializer also drops, so the two paths produce
/// byte-identical output. Pinned by the `canonical_kind_bytes_runtime_
/// matches_const` test below.
pub fn canonical_kind_bytes(name: &str, schema: &SchemaType) -> alloc::vec::Vec<u8> {
    let shape = crate::types::KindShape {
        name: alloc::borrow::Cow::Owned(name.into()),
        schema: schema_to_shape(schema),
    };
    postcard::to_allocvec(&shape).expect("canonical KindShape serialization is infallible")
}

fn schema_to_shape(s: &SchemaType) -> crate::types::SchemaShape {
    use crate::types::SchemaShape;
    match s {
        SchemaType::Unit => SchemaShape::Unit,
        SchemaType::Bool => SchemaShape::Bool,
        SchemaType::Scalar(p) => SchemaShape::Scalar(*p),
        SchemaType::String => SchemaShape::String,
        SchemaType::Bytes => SchemaShape::Bytes,
        SchemaType::Option(cell) => {
            SchemaShape::Option(alloc::boxed::Box::new(schema_to_shape(cell)))
        }
        SchemaType::Vec(cell) => SchemaShape::Vec(alloc::boxed::Box::new(schema_to_shape(cell))),
        SchemaType::Array { element, len } => SchemaShape::Array {
            element: alloc::boxed::Box::new(schema_to_shape(element)),
            len: *len,
        },
        SchemaType::Struct { fields, repr_c } => SchemaShape::Struct {
            fields: fields.iter().map(|f| schema_to_shape(&f.ty)).collect(),
            repr_c: *repr_c,
        },
        SchemaType::Enum { variants } => SchemaShape::Enum {
            variants: variants.iter().map(variant_to_shape).collect(),
        },
        SchemaType::Ref(cell) => SchemaShape::Ref(alloc::boxed::Box::new(schema_to_shape(cell))),
        SchemaType::Map { key, value } => SchemaShape::Map {
            key: alloc::boxed::Box::new(schema_to_shape(key)),
            value: alloc::boxed::Box::new(schema_to_shape(value)),
        },
    }
}

fn variant_to_shape(v: &EnumVariant) -> crate::types::VariantShape {
    use crate::types::VariantShape;
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

/// Domain tag prefixed to every kind-id hash input so the `Kind::ID`
/// space is disjoint from `MailboxId`. Must stay byte-identical to
/// `aether_mail::KIND_DOMAIN`; duplicated here for the same reason
/// `fnv1a_64` is (aether-mail depends on hub-protocol, not the
/// other way around).
pub(crate) const KIND_DOMAIN: &[u8] = b"kind:";

use crate::tag_bits::{HASH_MASK, TAG_KIND, TAG_SHIFT};

/// Derive a `Kind::ID` from a `(name, schema)` pair at runtime. Matches
/// the `#[derive(Kind)]` compile-time emission byte-for-byte:
/// `Tag::Kind` ORed into the high 4 bits + the low 60 bits of
/// `fnv1a_64_prefixed(KIND_DOMAIN, &canonical_kind_bytes(name, schema))`.
/// A substrate computing `kind_id_from_parts(&desc.name, &desc.schema)`
/// after a runtime `register_kind_with_descriptor` agrees with the id
/// the component published as `<K as Kind>::ID` (ADR-0030 Phase 2 +
/// ADR-0064).
pub fn kind_id_from_parts(name: &str, schema: &SchemaType) -> u64 {
    ((TAG_KIND as u64) << TAG_SHIFT)
        | (fnv1a_64_prefixed(KIND_DOMAIN, &canonical_kind_bytes(name, schema)) & HASH_MASK)
}

/// Derive a `Kind::ID` from a decoded `KindShape`. Same hash as
/// `kind_id_from_parts` — the canonical bytes format is
/// `postcard(KindShape)`, so we postcard the shape directly without a
/// `SchemaShape → SchemaType` detour. Used by `kind_manifest` to key
/// labels records by id after decoding both sections off the wasm.
pub fn kind_id_from_shape(shape: &crate::types::KindShape) -> u64 {
    let bytes =
        postcard::to_allocvec(shape).expect("canonical KindShape serialization is infallible");
    ((TAG_KIND as u64) << TAG_SHIFT) | (fnv1a_64_prefixed(KIND_DOMAIN, &bytes) & HASH_MASK)
}

/// FNV-1a 64 over `prefix ++ payload`, mirrored from
/// `aether_mail::fnv1a_64_prefixed`. Duplicated here because
/// `aether-mail` depends on `aether-hub-protocol`, not the other way
/// around — same offset basis and prime, identical output. Exposed at
/// crate scope so the hub's canonical-bytes path can hash
/// `KIND_DOMAIN ++ bytes` without a transient `Vec<u8>`.
pub(crate) const fn fnv1a_64_prefixed(prefix: &[u8], payload: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0;
    while i < prefix.len() {
        hash ^= prefix[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    let mut i = 0;
    while i < payload.len() {
        hash ^= payload[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    hash
}

const fn write_schema(schema: &SchemaType, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    match schema {
        SchemaType::Unit => {
            out[pos] = SCHEMA_UNIT;
            pos += 1;
        }
        SchemaType::Bool => {
            out[pos] = SCHEMA_BOOL;
            pos += 1;
        }
        SchemaType::Scalar(p) => {
            out[pos] = SCHEMA_SCALAR;
            pos += 1;
            out[pos] = primitive_tag(*p);
            pos += 1;
        }
        SchemaType::String => {
            out[pos] = SCHEMA_STRING;
            pos += 1;
        }
        SchemaType::Bytes => {
            out[pos] = SCHEMA_BYTES;
            pos += 1;
        }
        SchemaType::Option(cell) => {
            out[pos] = SCHEMA_OPTION;
            pos += 1;
            pos = write_cell(cell, out, pos);
        }
        SchemaType::Vec(cell) => {
            out[pos] = SCHEMA_VEC;
            pos += 1;
            pos = write_cell(cell, out, pos);
        }
        SchemaType::Array { element, len } => {
            out[pos] = SCHEMA_ARRAY;
            pos += 1;
            pos = write_cell(element, out, pos);
            pos = write_varint_u32(*len, out, pos);
        }
        SchemaType::Struct { fields, repr_c } => {
            let slice = cow_named_fields(fields);
            out[pos] = SCHEMA_STRUCT;
            pos += 1;
            pos = write_varint_usize(slice.len(), out, pos);
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
            out[pos] = SCHEMA_ENUM;
            pos += 1;
            pos = write_varint_usize(slice.len(), out, pos);
            let mut i = 0;
            while i < slice.len() {
                pos = write_variant(&slice[i], out, pos);
                i += 1;
            }
        }
        SchemaType::Ref(cell) => {
            out[pos] = SCHEMA_REF;
            pos += 1;
            pos = write_cell(cell, out, pos);
        }
        SchemaType::Map { key, value } => {
            out[pos] = SCHEMA_MAP;
            pos += 1;
            pos = write_cell(key, out, pos);
            pos = write_cell(value, out, pos);
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
            out[pos] = VARIANT_UNIT;
            pos += 1;
            pos = write_varint_u32(*discriminant, out, pos);
        }
        EnumVariant::Tuple {
            discriminant,
            fields,
            ..
        } => {
            let slice = cow_schema_types(fields);
            out[pos] = VARIANT_TUPLE;
            pos += 1;
            pos = write_varint_u32(*discriminant, out, pos);
            pos = write_varint_usize(slice.len(), out, pos);
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
            out[pos] = VARIANT_STRUCT;
            pos += 1;
            pos = write_varint_u32(*discriminant, out, pos);
            pos = write_varint_usize(slice.len(), out, pos);
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
