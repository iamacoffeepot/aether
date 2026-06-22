// Cast-shape helpers shared between the encode and decode paths.
//
// Both `encode_schema` and `decode_schema` walk the same `#[repr(C)]`
// byte layout for `Struct { repr_c: true }` schemas and need an
// alignment lookup for primitive scalars to pad / skip between fields.
// The function is byte-identical on both sides, so it lives here.

use aether_data::{Primitive, SchemaType};

/// Byte alignment of a `Primitive` scalar in its cast-shape (`#[repr(C)]`)
/// layout. Mirrors the alignment Rust would pick for the corresponding
/// scalar type.
pub fn align_of_primitive(p: Primitive) -> usize {
    match p {
        Primitive::U8 | Primitive::I8 => 1,
        Primitive::U16 | Primitive::I16 => 2,
        Primitive::U32 | Primitive::I32 | Primitive::F32 => 4,
        Primitive::U64 | Primitive::I64 | Primitive::F64 => 8,
    }
}

/// Error message reported when a non-cast `SchemaType` variant
/// (`Bool` / `String` / `Bytes` / `Option` / `Vec` / `Enum` / `Unit` /
/// `Map`) appears inside a `#[repr(C)]` cast-shaped struct.
/// Used by both the encode and decode paths so the diagnostic string
/// stays byte-identical across the two sides.
pub const NON_CAST_VARIANTS_MSG: &str = "non-cast field inside cast-shaped struct";

/// `Some(NON_CAST_VARIANTS_MSG)` when `ty` is one of the non-cast
/// `SchemaType` variants that can't live in cast-shape position;
/// `None` for the cast-eligible variants (`Scalar`, `TypeId`,
/// `Array`, and `Struct { repr_c: true }`).
///
/// Both the encode and decode paths previously open-coded the same
/// `Bool | String | Bytes | Option(_) | Vec(_) | Enum { .. } | Unit |
/// Map { .. }` OR-pattern with the same error message.
/// Centralising the classification here keeps the exhaustiveness
/// check (every new `SchemaType` variant forces a decision in this
/// function) while letting the call sites stay short.
#[must_use]
pub fn non_cast_variant_error(ty: &SchemaType) -> Option<&'static str> {
    match ty {
        SchemaType::Bool
        | SchemaType::String
        | SchemaType::Bytes
        | SchemaType::Option(_)
        | SchemaType::Vec(_)
        | SchemaType::Enum { .. }
        | SchemaType::Unit
        | SchemaType::Map { .. } => Some(NON_CAST_VARIANTS_MSG),
        SchemaType::Scalar(_)
        | SchemaType::TypeId(_)
        | SchemaType::Array { .. }
        | SchemaType::Struct { .. } => None,
    }
}
