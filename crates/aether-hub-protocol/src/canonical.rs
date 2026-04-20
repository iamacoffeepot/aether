//! Const-fn canonical serializer for `SchemaType` and `KindLabels`
//! (ADR-0032). Produces postcard-compatible bytes at const-eval time
//! so they can be embedded directly in `#[link_section]` statics and
//! hashed to derive `Kind::ID`.
//!
//! The canonical schema format matches postcard of `SchemaShape`
//! byte-for-byte: the only difference from `postcard(SchemaType)` is
//! that `NamedField.name` and `EnumVariant`'s `name` field are
//! omitted. Enum discriminant positions agree between `SchemaType`
//! and `SchemaShape` by construction (same arm order, same field
//! declaration order), so hub-side decode via
//! `postcard::from_bytes::<SchemaShape>` reads the canonical bytes
//! cleanly.
//!
//! Only `SchemaCell::Static` / `LabelCell::Static` variants are
//! legal in const context here. Derive-emitted schemas always use
//! `Static`; passing an `Owned` cell (or an `Owned` `Cow`) to these
//! const fns is a compile-time panic. Runtime consumers (the hub)
//! decode the produced bytes back into `Owned` cells via postcard.

use alloc::borrow::Cow;

use crate::types::{
    EnumVariant, KindLabels, LabelCell, LabelNode, NamedField, Primitive, SchemaCell, SchemaType,
    VariantLabel,
};

// ---- Cow accessors ---------------------------------------------------
//
// `Cow::Borrowed::deref` isn't const, so `&cow[i]` / `&cow.as_str()`
// can't be called from a const fn. Hand-roll a `match` per concrete
// slice/str type to narrow `Cow<'static, [T]>` to `&[T]` (or
// `Cow<str>` to `&str`). All panic on `Owned` — only the derive-
// emitted `Cow::Borrowed` path is legal at const-eval here.

const fn cow_named_fields<'a>(c: &'a Cow<'static, [NamedField]>) -> &'a [NamedField] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[NamedField]> not supported in const"),
    }
}

const fn cow_enum_variants<'a>(c: &'a Cow<'static, [EnumVariant]>) -> &'a [EnumVariant] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[EnumVariant]> not supported in const"),
    }
}

const fn cow_schema_types<'a>(c: &'a Cow<'static, [SchemaType]>) -> &'a [SchemaType] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[SchemaType]> not supported in const"),
    }
}

const fn cow_label_nodes<'a>(c: &'a Cow<'static, [LabelNode]>) -> &'a [LabelNode] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[LabelNode]> not supported in const"),
    }
}

const fn cow_variant_labels<'a>(c: &'a Cow<'static, [VariantLabel]>) -> &'a [VariantLabel] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[VariantLabel]> not supported in const"),
    }
}

const fn cow_strs<'a>(c: &'a Cow<'static, [Cow<'static, str>]>) -> &'a [Cow<'static, str>] {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<[Cow<str>]> not supported in const"),
    }
}

const fn cow_str_as_str<'a>(c: &'a Cow<'static, str>) -> &'a str {
    match c {
        Cow::Borrowed(s) => s,
        Cow::Owned(_) => panic!("canonical: Owned Cow<str> not supported in const"),
    }
}

// ---- tag constants ---------------------------------------------------
//
// Hand-pinned (not "source order"-inferred) so rearranging an enum in
// `types.rs` can't silently change the wire without the change showing
// up here too. Substrate/hub `SchemaShape` must keep the same ordering.

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

const LABEL_ANONYMOUS: u8 = 0;
const LABEL_OPTION: u8 = 1;
const LABEL_VEC: u8 = 2;
const LABEL_ARRAY: u8 = 3;
const LABEL_STRUCT: u8 = 4;
const LABEL_ENUM: u8 = 5;

const VARIANT_LABEL_UNIT: u8 = 0;
const VARIANT_LABEL_TUPLE: u8 = 1;
const VARIANT_LABEL_STRUCT: u8 = 2;

// ---- varint helpers --------------------------------------------------

/// Byte count of `val` in postcard's unsigned-varint encoding
/// (7 bits per byte, MSB set until the last byte).
pub const fn varint_u32_len(val: u32) -> usize {
    if val < (1 << 7) {
        1
    } else if val < (1 << 14) {
        2
    } else if val < (1 << 21) {
        3
    } else if val < (1 << 28) {
        4
    } else {
        5
    }
}

pub const fn varint_usize_len(val: usize) -> usize {
    if val > u32::MAX as usize {
        panic!("varint_usize_len: value exceeds u32::MAX");
    }
    varint_u32_len(val as u32)
}

const fn write_varint_u32(mut val: u32, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    while val >= 0x80 {
        out[pos] = ((val & 0x7F) as u8) | 0x80;
        val >>= 7;
        pos += 1;
    }
    out[pos] = val as u8;
    pos + 1
}

const fn write_varint_usize(val: usize, out: &mut [u8], cursor: usize) -> usize {
    if val > u32::MAX as usize {
        panic!("write_varint_usize: value exceeds u32::MAX");
    }
    write_varint_u32(val as u32, out, cursor)
}

const fn write_str(s: &str, out: &mut [u8], cursor: usize) -> usize {
    let bytes = s.as_bytes();
    let mut pos = write_varint_usize(bytes.len(), out, cursor);
    let mut i = 0;
    while i < bytes.len() {
        out[pos] = bytes[i];
        pos += 1;
        i += 1;
    }
    pos
}

const fn str_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    varint_usize_len(bytes.len()) + bytes.len()
}

// ---- SchemaType serializer ------------------------------------------

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
pub const fn canonical_serialize_kind<const N: usize>(
    name: &str,
    schema: &SchemaType,
) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_str(name, &mut out, 0);
    pos = write_schema(schema, &mut out, pos);
    if pos != N {
        panic!("canonical_serialize_kind: size mismatch between len pass and serialize pass");
    }
    out
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

// ---- Labels serializer ----------------------------------------------

/// Byte length for `KindLabels` postcard encoding.
pub const fn canonical_len_labels(labels: &KindLabels) -> usize {
    str_len(cow_str_as_str(&labels.kind_label)) + label_node_len(&labels.root)
}

const fn label_node_len(node: &LabelNode) -> usize {
    match node {
        LabelNode::Anonymous => 1,
        LabelNode::Option(cell) => 1 + label_cell_len(cell),
        LabelNode::Vec(cell) => 1 + label_cell_len(cell),
        LabelNode::Array(cell) => 1 + label_cell_len(cell),
        LabelNode::Struct {
            type_label,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            let mut total = 1 + option_str_len(type_label);
            total += varint_usize_len(names.len());
            let mut i = 0;
            while i < names.len() {
                total += str_len(cow_str_as_str(&names[i]));
                i += 1;
            }
            total += varint_usize_len(fs.len());
            let mut i = 0;
            while i < fs.len() {
                total += label_node_len(&fs[i]);
                i += 1;
            }
            total
        }
        LabelNode::Enum {
            type_label,
            variants,
        } => {
            let vs = cow_variant_labels(variants);
            let mut total = 1 + option_str_len(type_label);
            total += varint_usize_len(vs.len());
            let mut i = 0;
            while i < vs.len() {
                total += variant_label_len(&vs[i]);
                i += 1;
            }
            total
        }
    }
}

const fn label_cell_len(cell: &LabelCell) -> usize {
    match cell {
        LabelCell::Static(r) => label_node_len(r),
        LabelCell::Owned(_) => {
            panic!("canonical labels: Owned LabelCell not supported in const context");
        }
    }
}

const fn variant_label_len(v: &VariantLabel) -> usize {
    match v {
        VariantLabel::Unit { name } => 1 + str_len(cow_str_as_str(name)),
        VariantLabel::Tuple { name, fields } => {
            let fs = cow_label_nodes(fields);
            let mut total = 1 + str_len(cow_str_as_str(name)) + varint_usize_len(fs.len());
            let mut i = 0;
            while i < fs.len() {
                total += label_node_len(&fs[i]);
                i += 1;
            }
            total
        }
        VariantLabel::Struct {
            name,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            let mut total = 1 + str_len(cow_str_as_str(name));
            total += varint_usize_len(names.len());
            let mut i = 0;
            while i < names.len() {
                total += str_len(cow_str_as_str(&names[i]));
                i += 1;
            }
            total += varint_usize_len(fs.len());
            let mut i = 0;
            while i < fs.len() {
                total += label_node_len(&fs[i]);
                i += 1;
            }
            total
        }
    }
}

const fn option_str_len(s: &Option<Cow<'static, str>>) -> usize {
    match s {
        None => 1,
        Some(inner) => 1 + str_len(cow_str_as_str(inner)),
    }
}

/// Serialize `labels` into `N` bytes of canonical postcard form.
pub const fn canonical_serialize_labels<const N: usize>(labels: &KindLabels) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_str(cow_str_as_str(&labels.kind_label), &mut out, 0);
    pos = write_label_node(&labels.root, &mut out, pos);
    if pos != N {
        panic!("canonical_serialize_labels: size mismatch between len pass and serialize pass");
    }
    out
}

const fn write_label_node(node: &LabelNode, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    match node {
        LabelNode::Anonymous => {
            out[pos] = LABEL_ANONYMOUS;
            pos += 1;
        }
        LabelNode::Option(cell) => {
            out[pos] = LABEL_OPTION;
            pos += 1;
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Vec(cell) => {
            out[pos] = LABEL_VEC;
            pos += 1;
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Array(cell) => {
            out[pos] = LABEL_ARRAY;
            pos += 1;
            pos = write_label_cell(cell, out, pos);
        }
        LabelNode::Struct {
            type_label,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            out[pos] = LABEL_STRUCT;
            pos += 1;
            pos = write_option_str(type_label, out, pos);
            pos = write_varint_usize(names.len(), out, pos);
            let mut i = 0;
            while i < names.len() {
                pos = write_str(cow_str_as_str(&names[i]), out, pos);
                i += 1;
            }
            pos = write_varint_usize(fs.len(), out, pos);
            let mut i = 0;
            while i < fs.len() {
                pos = write_label_node(&fs[i], out, pos);
                i += 1;
            }
        }
        LabelNode::Enum {
            type_label,
            variants,
        } => {
            let vs = cow_variant_labels(variants);
            out[pos] = LABEL_ENUM;
            pos += 1;
            pos = write_option_str(type_label, out, pos);
            pos = write_varint_usize(vs.len(), out, pos);
            let mut i = 0;
            while i < vs.len() {
                pos = write_variant_label(&vs[i], out, pos);
                i += 1;
            }
        }
    }
    pos
}

const fn write_label_cell(cell: &LabelCell, out: &mut [u8], cursor: usize) -> usize {
    match cell {
        LabelCell::Static(r) => write_label_node(r, out, cursor),
        LabelCell::Owned(_) => {
            panic!("canonical labels: Owned LabelCell not supported in const context");
        }
    }
}

const fn write_variant_label(v: &VariantLabel, out: &mut [u8], cursor: usize) -> usize {
    let mut pos = cursor;
    match v {
        VariantLabel::Unit { name } => {
            out[pos] = VARIANT_LABEL_UNIT;
            pos += 1;
            pos = write_str(cow_str_as_str(name), out, pos);
        }
        VariantLabel::Tuple { name, fields } => {
            let fs = cow_label_nodes(fields);
            out[pos] = VARIANT_LABEL_TUPLE;
            pos += 1;
            pos = write_str(cow_str_as_str(name), out, pos);
            pos = write_varint_usize(fs.len(), out, pos);
            let mut i = 0;
            while i < fs.len() {
                pos = write_label_node(&fs[i], out, pos);
                i += 1;
            }
        }
        VariantLabel::Struct {
            name,
            field_names,
            fields,
        } => {
            let names = cow_strs(field_names);
            let fs = cow_label_nodes(fields);
            out[pos] = VARIANT_LABEL_STRUCT;
            pos += 1;
            pos = write_str(cow_str_as_str(name), out, pos);
            pos = write_varint_usize(names.len(), out, pos);
            let mut i = 0;
            while i < names.len() {
                pos = write_str(cow_str_as_str(&names[i]), out, pos);
                i += 1;
            }
            pos = write_varint_usize(fs.len(), out, pos);
            let mut i = 0;
            while i < fs.len() {
                pos = write_label_node(&fs[i], out, pos);
                i += 1;
            }
        }
    }
    pos
}

const fn write_option_str(
    s: &Option<Cow<'static, str>>,
    out: &mut [u8],
    cursor: usize,
) -> usize {
    let mut pos = cursor;
    match s {
        None => {
            out[pos] = 0;
            pos += 1;
        }
        Some(inner) => {
            out[pos] = 1;
            pos += 1;
            pos = write_str(cow_str_as_str(inner), out, pos);
        }
    }
    pos
}

#[cfg(test)]
mod tests {
    //! The contract these tests pin: canonical bytes round-trip through
    //! `postcard::from_bytes::<SchemaShape>` / `postcard::from_bytes::<KindLabels>`.
    //! That's what the hub relies on after reading `aether.kinds` /
    //! `aether.kinds.labels` sections. If these diverge, the hub can't
    //! decode what derives produce.
    //!
    //! Each test constructs a schema via `static` so `SchemaCell::Static`
    //! is reachable in const context, runs both passes, and compares
    //! against a hand-built `SchemaShape` that matches the stripped shape.
    use super::*;
    use crate::types::{
        EnumVariant, KindLabels, KindShape, LabelCell, LabelNode, NamedField, Primitive,
        SchemaCell, SchemaShape, SchemaType, VariantLabel, VariantShape,
    };

    static F32: SchemaType = SchemaType::Scalar(Primitive::F32);

    static VERTEX: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[
            NamedField {
                name: Cow::Borrowed("x"),
                ty: SchemaType::Scalar(Primitive::F32),
            },
            NamedField {
                name: Cow::Borrowed("y"),
                ty: SchemaType::Scalar(Primitive::F32),
            },
        ]),
        repr_c: true,
    };

    static TRIANGLE: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[NamedField {
            name: Cow::Borrowed("verts"),
            ty: SchemaType::Array {
                element: SchemaCell::Static(&VERTEX),
                len: 3,
            },
        }]),
        repr_c: true,
    };

    static RESULT: SchemaType = SchemaType::Enum {
        variants: Cow::Borrowed(&[
            EnumVariant::Unit {
                name: Cow::Borrowed("Pending"),
                discriminant: 0,
            },
            EnumVariant::Tuple {
                name: Cow::Borrowed("Ok"),
                discriminant: 1,
                fields: Cow::Borrowed(&[SchemaType::Scalar(Primitive::U64)]),
            },
            EnumVariant::Struct {
                name: Cow::Borrowed("Err"),
                discriminant: 2,
                fields: Cow::Borrowed(&[NamedField {
                    name: Cow::Borrowed("reason"),
                    ty: SchemaType::String,
                }]),
            },
        ]),
    };

    #[test]
    fn canonical_schema_primitive_round_trips_as_shape() {
        const N: usize = canonical_len_schema(&F32);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&F32);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(shape, SchemaShape::Scalar(Primitive::F32));
    }

    #[test]
    fn canonical_schema_struct_round_trips_as_shape() {
        const N: usize = canonical_len_schema(&VERTEX);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&VERTEX);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            shape,
            SchemaShape::Struct {
                fields: vec![
                    SchemaShape::Scalar(Primitive::F32),
                    SchemaShape::Scalar(Primitive::F32),
                ],
                repr_c: true,
            }
        );
    }

    #[test]
    fn canonical_schema_nested_array_of_struct_round_trips() {
        const N: usize = canonical_len_schema(&TRIANGLE);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&TRIANGLE);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        let expected = SchemaShape::Struct {
            fields: vec![SchemaShape::Array {
                element: Box::new(SchemaShape::Struct {
                    fields: vec![
                        SchemaShape::Scalar(Primitive::F32),
                        SchemaShape::Scalar(Primitive::F32),
                    ],
                    repr_c: true,
                }),
                len: 3,
            }],
            repr_c: true,
        };
        assert_eq!(shape, expected);
    }

    #[test]
    fn canonical_schema_enum_all_variants_round_trip() {
        const N: usize = canonical_len_schema(&RESULT);
        const BYTES: [u8; N] = canonical_serialize_schema::<N>(&RESULT);
        let shape: SchemaShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(
            shape,
            SchemaShape::Enum {
                variants: vec![
                    VariantShape::Unit { discriminant: 0 },
                    VariantShape::Tuple {
                        discriminant: 1,
                        fields: vec![SchemaShape::Scalar(Primitive::U64)],
                    },
                    VariantShape::Struct {
                        discriminant: 2,
                        fields: vec![SchemaShape::String],
                    },
                ],
            }
        );
    }

    #[test]
    fn canonical_kind_round_trips_as_kindshape() {
        const NAME: &str = "test.triangle";
        const N: usize = canonical_len_kind(NAME, &TRIANGLE);
        const BYTES: [u8; N] = canonical_serialize_kind::<N>(NAME, &TRIANGLE);
        let shape: KindShape = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(shape.name, "test.triangle");
        let SchemaShape::Struct { fields, repr_c } = &shape.schema else {
            panic!("expected Struct");
        };
        assert!(*repr_c);
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn canonical_schema_two_equal_shapes_produce_equal_bytes() {
        // Two schemas with identical wire shape but different field
        // names must produce identical canonical bytes. This pins the
        // structural-not-nominal hashing invariant from ADR-0032.
        static V1: SchemaType = SchemaType::Struct {
            fields: Cow::Borrowed(&[
                NamedField {
                    name: Cow::Borrowed("x"),
                    ty: SchemaType::Scalar(Primitive::F32),
                },
                NamedField {
                    name: Cow::Borrowed("y"),
                    ty: SchemaType::Scalar(Primitive::F32),
                },
            ]),
            repr_c: true,
        };
        static V2: SchemaType = SchemaType::Struct {
            fields: Cow::Borrowed(&[
                NamedField {
                    name: Cow::Borrowed("row"),
                    ty: SchemaType::Scalar(Primitive::F32),
                },
                NamedField {
                    name: Cow::Borrowed("col"),
                    ty: SchemaType::Scalar(Primitive::F32),
                },
            ]),
            repr_c: true,
        };
        const N1: usize = canonical_len_schema(&V1);
        const N2: usize = canonical_len_schema(&V2);
        const B1: [u8; N1] = canonical_serialize_schema::<N1>(&V1);
        const B2: [u8; N2] = canonical_serialize_schema::<N2>(&V2);
        assert_eq!(&B1[..], &B2[..]);
    }

    // Labels tests — these exercise the full `KindLabels` round-trip.

    static VERTEX_LABELS: LabelNode = LabelNode::Struct {
        type_label: Some(Cow::Borrowed("my_crate::Vertex")),
        field_names: Cow::Borrowed(&[Cow::Borrowed("x"), Cow::Borrowed("y")]),
        fields: Cow::Borrowed(&[LabelNode::Anonymous, LabelNode::Anonymous]),
    };

    static TRIANGLE_LABELS: KindLabels = KindLabels {
        kind_label: Cow::Borrowed("my_crate::Triangle"),
        root: LabelNode::Struct {
            type_label: Some(Cow::Borrowed("my_crate::Triangle")),
            field_names: Cow::Borrowed(&[Cow::Borrowed("verts")]),
            fields: Cow::Borrowed(&[LabelNode::Array(LabelCell::Static(&VERTEX_LABELS))]),
        },
    };

    #[test]
    fn canonical_labels_round_trip_via_postcard() {
        const N: usize = canonical_len_labels(&TRIANGLE_LABELS);
        const BYTES: [u8; N] = canonical_serialize_labels::<N>(&TRIANGLE_LABELS);
        let decoded: KindLabels = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, TRIANGLE_LABELS);
    }

    static RESULT_LABELS: KindLabels = KindLabels {
        kind_label: Cow::Borrowed("my_crate::Result"),
        root: LabelNode::Enum {
            type_label: Some(Cow::Borrowed("my_crate::Result")),
            variants: Cow::Borrowed(&[
                VariantLabel::Unit {
                    name: Cow::Borrowed("Pending"),
                },
                VariantLabel::Tuple {
                    name: Cow::Borrowed("Ok"),
                    fields: Cow::Borrowed(&[LabelNode::Anonymous]),
                },
                VariantLabel::Struct {
                    name: Cow::Borrowed("Err"),
                    field_names: Cow::Borrowed(&[Cow::Borrowed("reason")]),
                    fields: Cow::Borrowed(&[LabelNode::Anonymous]),
                },
            ]),
        },
    };

    #[test]
    fn canonical_labels_enum_round_trips() {
        const N: usize = canonical_len_labels(&RESULT_LABELS);
        const BYTES: [u8; N] = canonical_serialize_labels::<N>(&RESULT_LABELS);
        let decoded: KindLabels = postcard::from_bytes(&BYTES).expect("decode");
        assert_eq!(decoded, RESULT_LABELS);
    }
}
