// Cast-shape helpers shared between the encode and decode paths.
//
// Both `encode_schema` and `decode_schema` walk the same `#[repr(C)]`
// byte layout for `Struct { repr_c: true }` schemas and need an
// alignment lookup for primitive scalars to pad / skip between fields.
// The function is byte-identical on both sides, so it lives here.

use aether_data::Primitive;

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
/// `Ref` / `Map`) appears inside a `#[repr(C)]` cast-shaped struct.
/// Used by both the encode and decode paths so the diagnostic string
/// stays byte-identical across the two sides.
pub const NON_CAST_VARIANTS_MSG: &str = "non-cast field inside cast-shaped struct";
