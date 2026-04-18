// POD encoder: serde_json params + KindDescriptor field list → bytes
// matching the Rust `#[repr(C)]` layout the engine expects.
//
// Pure function; no hub state, no async.
//
// Layout rules implemented:
//   - Scalar fields emit LE bytes at an offset padded up to the
//     primitive's natural alignment (1/2/4/8).
//   - Array fields emit elements contiguously; the start is padded to
//     the element's alignment, elements don't re-pad between each
//     other (matches `[T; N]` layout).
//   - After all fields, trailing zeros bring total size up to a
//     multiple of the largest field alignment (Rust `#[repr(C)]`
//     struct size rule).
//
// ADR-0019 PR 4 added `encode_schema`, the `SchemaType`-driven sibling
// of `encode_pod` for `KindEncoding::Schema(...)` kinds. PR 5 wired
// the postcard path through it for postcard-shaped schemas (`String`,
// `Bytes`, `Vec`, `Option`, `Enum`, `Struct { repr_c: false }`).
//
// Wire format for the postcard path follows postcard 1.x exactly:
//   - bool: 1 byte (0 or 1)
//   - u8/i8: 1 byte
//   - u16..u64: LEB128 varint
//   - i16..i64: zigzag-then-LEB128
//   - f32/f64: little-endian
//   - String / &[u8] (Bytes): varint length + bytes
//   - Vec<T>: varint length + concatenated encoded elements
//   - [T; N]: concatenated encoded elements (no length prefix)
//   - Option<T>: 1-byte tag (0 or 1), then T if Some
//   - enum: varint discriminant, then variant body in declaration order
//   - struct: concatenated field bytes in declaration order
// We write the bytes directly rather than going through a serde
// serializer because the JSON-driven encoding is structural — matching
// postcard's wire format byte-for-byte is the contract, not "calling
// postcard".

use std::fmt;

use aether_hub_protocol::{
    EnumVariant, NamedField, PodField, PodFieldType, PodPrimitive, Primitive, SchemaType,
};
use serde_json::Value;

#[derive(Debug)]
pub enum EncodeError {
    NotAnObject,
    MissingField(String),
    UnexpectedField(String),
    TypeMismatch {
        field: String,
        expected: &'static str,
    },
    OutOfRange {
        field: String,
        reason: String,
    },
    ArrayLengthMismatch {
        field: String,
        expected: u32,
        got: usize,
    },
    /// A schema arm the hub encoder can't handle in this position.
    /// PR 5's postcard path covers every top-level `SchemaType`, so
    /// this variant only fires for fields-inside-cast-structs that
    /// disqualify the parent from cast eligibility. Carries a short
    /// description so the agent error is actionable.
    UnsupportedSchema(&'static str),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::NotAnObject => write!(f, "params must be a JSON object"),
            EncodeError::MissingField(name) => write!(f, "missing required field {name:?}"),
            EncodeError::UnexpectedField(name) => {
                write!(f, "unexpected field {name:?} not in descriptor")
            }
            EncodeError::TypeMismatch { field, expected } => {
                write!(f, "field {field:?} expected {expected}")
            }
            EncodeError::OutOfRange { field, reason } => write!(f, "field {field:?}: {reason}"),
            EncodeError::ArrayLengthMismatch {
                field,
                expected,
                got,
            } => write!(
                f,
                "field {field:?}: array length {got} != expected {expected}"
            ),
            EncodeError::UnsupportedSchema(shape) => {
                write!(f, "schema arm not supported by hub encoder: {shape}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Encode `params` into the byte layout implied by `fields`. Params
/// is expected to be a JSON object; keys not present in the
/// descriptor are rejected so typos don't silently drop data.
pub fn encode_pod(params: &Value, fields: &[PodField]) -> Result<Vec<u8>, EncodeError> {
    let obj = params.as_object().ok_or(EncodeError::NotAnObject)?;

    for key in obj.keys() {
        if !fields.iter().any(|f| &f.name == key) {
            return Err(EncodeError::UnexpectedField(key.clone()));
        }
    }

    let mut out = Vec::new();
    let mut max_align = 1usize;

    for field in fields {
        let value = obj
            .get(&field.name)
            .ok_or_else(|| EncodeError::MissingField(field.name.clone()))?;
        match &field.ty {
            PodFieldType::Scalar(p) => {
                let a = align_of(*p);
                pad_to(&mut out, a);
                write_primitive(&mut out, *p, value, &field.name)?;
                max_align = max_align.max(a);
            }
            PodFieldType::Array { element, len } => {
                let a = align_of(*element);
                pad_to(&mut out, a);
                let arr = value.as_array().ok_or_else(|| EncodeError::TypeMismatch {
                    field: field.name.clone(),
                    expected: "array",
                })?;
                if arr.len() != *len as usize {
                    return Err(EncodeError::ArrayLengthMismatch {
                        field: field.name.clone(),
                        expected: *len,
                        got: arr.len(),
                    });
                }
                for (i, v) in arr.iter().enumerate() {
                    let elem_name = format!("{}[{}]", field.name, i);
                    write_primitive(&mut out, *element, v, &elem_name)?;
                }
                max_align = max_align.max(a);
            }
        }
    }

    pad_to(&mut out, max_align);
    Ok(out)
}

/// ADR-0019: encode `params` against a `SchemaType` descriptor.
/// Dispatches on the schema's wire shape:
///
/// - `Unit` → empty payload.
/// - `Struct { repr_c: true }` (and the recursive cast-shaped tree
///   under it) → `#[repr(C)]` byte layout, decodable by
///   `bytemuck::cast` on the substrate side. Same wire bytes as
///   `encode_pod` for the same logical shape.
/// - Everything else → postcard wire format, written directly per the
///   format described at the top of this file.
pub fn encode_schema(params: &Value, schema: &SchemaType) -> Result<Vec<u8>, EncodeError> {
    match schema {
        SchemaType::Unit => {
            // Empty payload. Match `encode_pod`'s behavior of accepting
            // an empty object or null; reject explicit non-empty input
            // so a typo doesn't get silently swallowed.
            if let Some(obj) = params.as_object()
                && !obj.is_empty()
            {
                return Err(EncodeError::UnexpectedField(
                    obj.keys().next().cloned().unwrap_or_default(),
                ));
            }
            Ok(Vec::new())
        }
        SchemaType::Struct {
            fields,
            repr_c: true,
        } => {
            let obj = params.as_object().ok_or(EncodeError::NotAnObject)?;
            for key in obj.keys() {
                if !fields.iter().any(|f| &f.name == key) {
                    return Err(EncodeError::UnexpectedField(key.clone()));
                }
            }
            let mut out = Vec::new();
            let max_align = encode_struct_fields(&mut out, obj, fields)?;
            pad_to(&mut out, max_align);
            Ok(out)
        }
        // Postcard path: every non-cast schema. The walker handles
        // top-level scalars / strings / vecs / enums uniformly with
        // their nested counterparts.
        _ => {
            let mut out = Vec::new();
            encode_postcard(params, schema, "$", &mut out)?;
            Ok(out)
        }
    }
}

/// Recursively encode `value` into postcard wire format under `schema`.
/// `path` is a dotted breadcrumb (`$.field.subfield[2]`) used to make
/// error messages locate the offending field in deeply-nested params.
fn encode_postcard(
    value: &Value,
    schema: &SchemaType,
    path: &str,
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    match schema {
        SchemaType::Unit => Ok(()),
        SchemaType::Bool => {
            let b = value.as_bool().ok_or_else(|| EncodeError::TypeMismatch {
                field: path.to_owned(),
                expected: "bool",
            })?;
            out.push(b as u8);
            Ok(())
        }
        SchemaType::Scalar(p) => write_scalar_postcard(*p, value, path, out),
        SchemaType::String => {
            let s = value.as_str().ok_or_else(|| EncodeError::TypeMismatch {
                field: path.to_owned(),
                expected: "string",
            })?;
            write_varint_u64(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
            Ok(())
        }
        SchemaType::Bytes => {
            let arr = value.as_array().ok_or_else(|| EncodeError::TypeMismatch {
                field: path.to_owned(),
                expected: "byte array",
            })?;
            write_varint_u64(out, arr.len() as u64);
            for (i, v) in arr.iter().enumerate() {
                let n = as_unsigned(v, path, "u8")?;
                let b: u8 = n
                    .try_into()
                    .map_err(|_| oor(&format!("{path}[{i}]"), "u8"))?;
                out.push(b);
            }
            Ok(())
        }
        SchemaType::Option(inner) => {
            if value.is_null() {
                out.push(0);
            } else {
                out.push(1);
                encode_postcard(value, inner, path, out)?;
            }
            Ok(())
        }
        SchemaType::Vec(inner) => {
            let arr = value.as_array().ok_or_else(|| EncodeError::TypeMismatch {
                field: path.to_owned(),
                expected: "array",
            })?;
            write_varint_u64(out, arr.len() as u64);
            for (i, v) in arr.iter().enumerate() {
                let elem_path = format!("{path}[{i}]");
                encode_postcard(v, inner, &elem_path, out)?;
            }
            Ok(())
        }
        SchemaType::Array { element, len } => {
            let arr = value.as_array().ok_or_else(|| EncodeError::TypeMismatch {
                field: path.to_owned(),
                expected: "array",
            })?;
            if arr.len() != *len as usize {
                return Err(EncodeError::ArrayLengthMismatch {
                    field: path.to_owned(),
                    expected: *len,
                    got: arr.len(),
                });
            }
            for (i, v) in arr.iter().enumerate() {
                let elem_path = format!("{path}[{i}]");
                encode_postcard(v, element, &elem_path, out)?;
            }
            Ok(())
        }
        SchemaType::Struct { fields, .. } => {
            // Postcard struct: concatenated field bytes in declaration
            // order. Reject unexpected keys (typo defense) and require
            // every field to be present.
            let obj = value.as_object().ok_or_else(|| EncodeError::TypeMismatch {
                field: path.to_owned(),
                expected: "object",
            })?;
            for key in obj.keys() {
                if !fields.iter().any(|f| &f.name == key) {
                    return Err(EncodeError::UnexpectedField(format!("{path}.{key}")));
                }
            }
            for field in fields {
                let v = obj
                    .get(&field.name)
                    .ok_or_else(|| EncodeError::MissingField(format!("{path}.{}", field.name)))?;
                let field_path = format!("{path}.{}", field.name);
                encode_postcard(v, &field.ty, &field_path, out)?;
            }
            Ok(())
        }
        SchemaType::Enum { variants } => {
            // Externally-tagged JSON: `{"VariantName": <body>}` for
            // tuple/struct variants, `"VariantName"` (string) for unit
            // variants. Same shape serde emits by default.
            let (tag, body) = decode_enum_tag(value, path)?;
            let variant = variants.iter().find(|v| variant_name(v) == tag).ok_or(
                EncodeError::TypeMismatch {
                    field: path.to_owned(),
                    expected: "enum variant matching schema",
                },
            )?;
            write_varint_u64(out, variant_discriminant(variant) as u64);
            encode_enum_body(body, variant, path, out)?;
            Ok(())
        }
    }
}

fn write_scalar_postcard(
    p: Primitive,
    v: &Value,
    name: &str,
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    match p {
        Primitive::U8 => {
            let n = as_unsigned(v, name, "u8")?;
            let n: u8 = n.try_into().map_err(|_| oor(name, "u8"))?;
            out.push(n);
        }
        Primitive::U16 => {
            let n = as_unsigned(v, name, "u16")?;
            let _: u16 = n.try_into().map_err(|_| oor(name, "u16"))?;
            write_varint_u64(out, n);
        }
        Primitive::U32 => {
            let n = as_unsigned(v, name, "u32")?;
            let _: u32 = n.try_into().map_err(|_| oor(name, "u32"))?;
            write_varint_u64(out, n);
        }
        Primitive::U64 => {
            let n = as_unsigned(v, name, "u64")?;
            write_varint_u64(out, n);
        }
        Primitive::I8 => {
            let n = as_signed(v, name, "i8")?;
            let n: i8 = n.try_into().map_err(|_| oor(name, "i8"))?;
            out.push(n as u8);
        }
        Primitive::I16 => {
            let n = as_signed(v, name, "i16")?;
            let n: i16 = n.try_into().map_err(|_| oor(name, "i16"))?;
            write_varint_u64(out, zigzag_i64(n as i64));
        }
        Primitive::I32 => {
            let n = as_signed(v, name, "i32")?;
            let n: i32 = n.try_into().map_err(|_| oor(name, "i32"))?;
            write_varint_u64(out, zigzag_i64(n as i64));
        }
        Primitive::I64 => {
            let n = as_signed(v, name, "i64")?;
            write_varint_u64(out, zigzag_i64(n));
        }
        Primitive::F32 => {
            let n = v.as_f64().ok_or_else(|| EncodeError::TypeMismatch {
                field: name.to_owned(),
                expected: "f32",
            })?;
            out.extend_from_slice(&(n as f32).to_le_bytes());
        }
        Primitive::F64 => {
            let n = v.as_f64().ok_or_else(|| EncodeError::TypeMismatch {
                field: name.to_owned(),
                expected: "f64",
            })?;
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
    Ok(())
}

/// LEB128-style varint write. Postcard 1.x uses this for u16/u32/u64
/// and (zigzagged) for i16/i32/i64, plus all collection lengths.
fn write_varint_u64(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn zigzag_i64(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

/// Pull `(tag, body)` out of an enum-shaped JSON value. Accepts:
///   - `"Variant"` (string) — unit variant.
///   - `{"Variant": body}` — single-key object — tuple or struct
///     variant. Body is whatever the variant's schema expects.
fn decode_enum_tag<'a>(value: &'a Value, path: &str) -> Result<(&'a str, &'a Value), EncodeError> {
    if let Some(s) = value.as_str() {
        return Ok((s, &Value::Null));
    }
    let obj = value.as_object().ok_or_else(|| EncodeError::TypeMismatch {
        field: path.to_owned(),
        expected: "enum (string or single-key object)",
    })?;
    if obj.len() != 1 {
        return Err(EncodeError::TypeMismatch {
            field: path.to_owned(),
            expected: "enum object with exactly one tag key",
        });
    }
    let (tag, body) = obj.iter().next().expect("len == 1");
    Ok((tag.as_str(), body))
}

fn variant_name(v: &EnumVariant) -> &str {
    match v {
        EnumVariant::Unit { name, .. }
        | EnumVariant::Tuple { name, .. }
        | EnumVariant::Struct { name, .. } => name.as_str(),
    }
}

fn variant_discriminant(v: &EnumVariant) -> u32 {
    match v {
        EnumVariant::Unit { discriminant, .. }
        | EnumVariant::Tuple { discriminant, .. }
        | EnumVariant::Struct { discriminant, .. } => *discriminant,
    }
}

fn encode_enum_body(
    body: &Value,
    variant: &EnumVariant,
    path: &str,
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    match variant {
        EnumVariant::Unit { .. } => {
            // Body should be Null (or absent — the string-tag form).
            // Anything else is suspicious; reject so a typo'd
            // {"Pending": {...}} doesn't get silently dropped.
            if !body.is_null() {
                return Err(EncodeError::TypeMismatch {
                    field: path.to_owned(),
                    expected: "unit variant has no body — pass the variant name as a bare string",
                });
            }
            Ok(())
        }
        EnumVariant::Tuple { fields, name, .. } => {
            // Tuple variant body is a JSON array (one entry per
            // tuple field), or — for a single-element tuple — the
            // element value directly. Serde's external tagging does
            // both interchangeably.
            if fields.len() == 1 {
                let nested_path = format!("{path}::{name}.0");
                encode_postcard(body, &fields[0], &nested_path, out)
            } else {
                let arr = body.as_array().ok_or_else(|| EncodeError::TypeMismatch {
                    field: path.to_owned(),
                    expected: "tuple variant body as array",
                })?;
                if arr.len() != fields.len() {
                    return Err(EncodeError::ArrayLengthMismatch {
                        field: path.to_owned(),
                        expected: fields.len() as u32,
                        got: arr.len(),
                    });
                }
                for (i, (v, ty)) in arr.iter().zip(fields.iter()).enumerate() {
                    let nested = format!("{path}::{name}.{i}");
                    encode_postcard(v, ty, &nested, out)?;
                }
                Ok(())
            }
        }
        EnumVariant::Struct { fields, name, .. } => {
            let obj = body.as_object().ok_or_else(|| EncodeError::TypeMismatch {
                field: path.to_owned(),
                expected: "struct variant body as object",
            })?;
            for key in obj.keys() {
                if !fields.iter().any(|f| &f.name == key) {
                    return Err(EncodeError::UnexpectedField(format!(
                        "{path}::{name}.{key}"
                    )));
                }
            }
            for field in fields {
                let v = obj.get(&field.name).ok_or_else(|| {
                    EncodeError::MissingField(format!("{path}::{name}.{}", field.name))
                })?;
                let nested = format!("{path}::{name}.{}", field.name);
                encode_postcard(v, &field.ty, &nested, out)?;
            }
            Ok(())
        }
    }
}

/// Recursively walk a `repr_c: true` struct's fields, packing them
/// into `out` with `#[repr(C)]` alignment rules. Returns the maximum
/// field alignment so the caller can apply trailing padding.
fn encode_struct_fields(
    out: &mut Vec<u8>,
    obj: &serde_json::Map<String, Value>,
    fields: &[NamedField],
) -> Result<usize, EncodeError> {
    let mut max_align = 1usize;
    for field in fields {
        let value = obj
            .get(&field.name)
            .ok_or_else(|| EncodeError::MissingField(field.name.clone()))?;
        let a = encode_field_value(out, &field.name, &field.ty, value)?;
        max_align = max_align.max(a);
    }
    Ok(max_align)
}

/// Encode one field value into `out`. Returns the alignment requirement
/// the field imposed (so the parent can track `max_align`). Recurses
/// into nested cast structs; rejects any non-cast leaf type with
/// `UnsupportedSchema`.
fn encode_field_value(
    out: &mut Vec<u8>,
    name: &str,
    ty: &SchemaType,
    value: &Value,
) -> Result<usize, EncodeError> {
    match ty {
        SchemaType::Scalar(p) => {
            let a = align_of_primitive(*p);
            pad_to(out, a);
            write_primitive_schema(out, *p, value, name)?;
            Ok(a)
        }
        SchemaType::Array { element, len } => {
            let arr = value.as_array().ok_or_else(|| EncodeError::TypeMismatch {
                field: name.to_owned(),
                expected: "array",
            })?;
            if arr.len() != *len as usize {
                return Err(EncodeError::ArrayLengthMismatch {
                    field: name.to_owned(),
                    expected: *len,
                    got: arr.len(),
                });
            }
            // Compute element alignment up-front and pad the start
            // before the first element. Subsequent elements are
            // contiguous (no per-element re-pad) — matches `[T; N]`
            // layout under `#[repr(C)]`.
            let elem_align = alignment_of_schema(element)?;
            pad_to(out, elem_align);
            for (i, v) in arr.iter().enumerate() {
                let elem_name = format!("{name}[{i}]");
                encode_field_value(out, &elem_name, element, v)?;
            }
            Ok(elem_align)
        }
        SchemaType::Struct {
            fields,
            repr_c: true,
        } => {
            // Nested cast struct — pad to its alignment, encode in
            // place, apply trailing padding so the next sibling field
            // starts at the right offset.
            let nested_align = alignment_of_schema(ty)?;
            pad_to(out, nested_align);
            let obj = value.as_object().ok_or_else(|| EncodeError::TypeMismatch {
                field: name.to_owned(),
                expected: "object",
            })?;
            for key in obj.keys() {
                if !fields.iter().any(|f| &f.name == key) {
                    return Err(EncodeError::UnexpectedField(format!("{name}.{key}")));
                }
            }
            let inner_max = encode_struct_fields(out, obj, fields)?;
            pad_to(out, inner_max);
            Ok(nested_align)
        }
        SchemaType::Struct { repr_c: false, .. } => Err(EncodeError::UnsupportedSchema(
            "Struct { repr_c: false } in cast-shaped parent",
        )),
        SchemaType::Bool
        | SchemaType::String
        | SchemaType::Bytes
        | SchemaType::Option(_)
        | SchemaType::Vec(_)
        | SchemaType::Enum { .. }
        | SchemaType::Unit => Err(EncodeError::UnsupportedSchema(
            "non-cast field inside cast-shaped struct",
        )),
    }
}

/// Compute the `#[repr(C)]` alignment of a cast-shaped schema. Used to
/// place fields at the right offsets without actually encoding them.
fn alignment_of_schema(ty: &SchemaType) -> Result<usize, EncodeError> {
    match ty {
        SchemaType::Scalar(p) => Ok(align_of_primitive(*p)),
        SchemaType::Array { element, .. } => alignment_of_schema(element),
        SchemaType::Struct {
            fields,
            repr_c: true,
        } => {
            let mut a = 1usize;
            for f in fields {
                a = a.max(alignment_of_schema(&f.ty)?);
            }
            Ok(a)
        }
        _ => Err(EncodeError::UnsupportedSchema(
            "alignment query on non-cast schema",
        )),
    }
}

fn align_of_primitive(p: Primitive) -> usize {
    match p {
        Primitive::U8 | Primitive::I8 => 1,
        Primitive::U16 | Primitive::I16 => 2,
        Primitive::U32 | Primitive::I32 | Primitive::F32 => 4,
        Primitive::U64 | Primitive::I64 | Primitive::F64 => 8,
    }
}

/// `Primitive`-flavored sibling of `write_primitive`. Same wire bytes
/// — both enums describe the same set of types — but the duplication
/// stays here until `PodPrimitive` goes away in the cleanup PR.
fn write_primitive_schema(
    out: &mut Vec<u8>,
    p: Primitive,
    v: &Value,
    name: &str,
) -> Result<(), EncodeError> {
    match p {
        Primitive::U8 => {
            let n = as_unsigned(v, name, "u8")?;
            let n: u8 = n.try_into().map_err(|_| oor(name, "u8"))?;
            out.push(n);
        }
        Primitive::U16 => {
            let n = as_unsigned(v, name, "u16")?;
            let n: u16 = n.try_into().map_err(|_| oor(name, "u16"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        Primitive::U32 => {
            let n = as_unsigned(v, name, "u32")?;
            let n: u32 = n.try_into().map_err(|_| oor(name, "u32"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        Primitive::U64 => {
            let n = as_unsigned(v, name, "u64")?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        Primitive::I8 => {
            let n = as_signed(v, name, "i8")?;
            let n: i8 = n.try_into().map_err(|_| oor(name, "i8"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        Primitive::I16 => {
            let n = as_signed(v, name, "i16")?;
            let n: i16 = n.try_into().map_err(|_| oor(name, "i16"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        Primitive::I32 => {
            let n = as_signed(v, name, "i32")?;
            let n: i32 = n.try_into().map_err(|_| oor(name, "i32"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        Primitive::I64 => {
            let n = as_signed(v, name, "i64")?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        Primitive::F32 => {
            let n = v.as_f64().ok_or_else(|| EncodeError::TypeMismatch {
                field: name.to_owned(),
                expected: "f32",
            })?;
            out.extend_from_slice(&(n as f32).to_le_bytes());
        }
        Primitive::F64 => {
            let n = v.as_f64().ok_or_else(|| EncodeError::TypeMismatch {
                field: name.to_owned(),
                expected: "f64",
            })?;
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
    Ok(())
}

fn pad_to(out: &mut Vec<u8>, align: usize) {
    while !out.len().is_multiple_of(align) {
        out.push(0);
    }
}

fn align_of(p: PodPrimitive) -> usize {
    match p {
        PodPrimitive::U8 | PodPrimitive::I8 => 1,
        PodPrimitive::U16 | PodPrimitive::I16 => 2,
        PodPrimitive::U32 | PodPrimitive::I32 | PodPrimitive::F32 => 4,
        PodPrimitive::U64 | PodPrimitive::I64 | PodPrimitive::F64 => 8,
    }
}

fn write_primitive(
    out: &mut Vec<u8>,
    p: PodPrimitive,
    v: &Value,
    name: &str,
) -> Result<(), EncodeError> {
    match p {
        PodPrimitive::U8 => {
            let n = as_unsigned(v, name, "u8")?;
            let n: u8 = n.try_into().map_err(|_| oor(name, "u8"))?;
            out.push(n);
        }
        PodPrimitive::U16 => {
            let n = as_unsigned(v, name, "u16")?;
            let n: u16 = n.try_into().map_err(|_| oor(name, "u16"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        PodPrimitive::U32 => {
            let n = as_unsigned(v, name, "u32")?;
            let n: u32 = n.try_into().map_err(|_| oor(name, "u32"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        PodPrimitive::U64 => {
            let n = as_unsigned(v, name, "u64")?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        PodPrimitive::I8 => {
            let n = as_signed(v, name, "i8")?;
            let n: i8 = n.try_into().map_err(|_| oor(name, "i8"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        PodPrimitive::I16 => {
            let n = as_signed(v, name, "i16")?;
            let n: i16 = n.try_into().map_err(|_| oor(name, "i16"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        PodPrimitive::I32 => {
            let n = as_signed(v, name, "i32")?;
            let n: i32 = n.try_into().map_err(|_| oor(name, "i32"))?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        PodPrimitive::I64 => {
            let n = as_signed(v, name, "i64")?;
            out.extend_from_slice(&n.to_le_bytes());
        }
        PodPrimitive::F32 => {
            let n = v.as_f64().ok_or_else(|| EncodeError::TypeMismatch {
                field: name.to_owned(),
                expected: "f32",
            })?;
            out.extend_from_slice(&(n as f32).to_le_bytes());
        }
        PodPrimitive::F64 => {
            let n = v.as_f64().ok_or_else(|| EncodeError::TypeMismatch {
                field: name.to_owned(),
                expected: "f64",
            })?;
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
    Ok(())
}

fn as_unsigned(v: &Value, name: &str, expected: &'static str) -> Result<u64, EncodeError> {
    v.as_u64().ok_or_else(|| EncodeError::TypeMismatch {
        field: name.to_owned(),
        expected,
    })
}

fn as_signed(v: &Value, name: &str, expected: &'static str) -> Result<i64, EncodeError> {
    v.as_i64().ok_or_else(|| EncodeError::TypeMismatch {
        field: name.to_owned(),
        expected,
    })
}

fn oor(name: &str, ty: &str) -> EncodeError {
    EncodeError::OutOfRange {
        field: name.to_owned(),
        reason: format!("out of range for {ty}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scalar(name: &str, ty: PodPrimitive) -> PodField {
        PodField {
            name: name.into(),
            ty: PodFieldType::Scalar(ty),
        }
    }

    fn array(name: &str, element: PodPrimitive, len: u32) -> PodField {
        PodField {
            name: name.into(),
            ty: PodFieldType::Array { element, len },
        }
    }

    #[test]
    fn key_u32_field() {
        let fields = &[scalar("code", PodPrimitive::U32)];
        let bytes = encode_pod(&json!({"code": 42}), fields).unwrap();
        assert_eq!(bytes, vec![42, 0, 0, 0]);
    }

    #[test]
    fn mouse_move_two_f32() {
        let fields = &[
            scalar("x", PodPrimitive::F32),
            scalar("y", PodPrimitive::F32),
        ];
        let bytes = encode_pod(&json!({"x": 1.5, "y": -3.25}), fields).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&1.5f32.to_le_bytes());
        expected.extend_from_slice(&(-3.25f32).to_le_bytes());
        assert_eq!(bytes, expected);
    }

    #[test]
    fn bytemuck_roundtrip_for_key_shape() {
        // Re-decoding our bytes via bytemuck::cast (as the engine
        // would) is the load-bearing proof of layout correctness.
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, Debug, PartialEq)]
        struct Key {
            code: u32,
        }
        let fields = &[scalar("code", PodPrimitive::U32)];
        let bytes = encode_pod(&json!({"code": 0xdead_beefu32}), fields).unwrap();
        let back: Key = bytemuck::cast_slice(&bytes)[0];
        assert_eq!(back, Key { code: 0xdead_beef });
    }

    #[test]
    fn bytemuck_roundtrip_for_mousemove_shape() {
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, Debug, PartialEq)]
        struct MouseMove {
            x: f32,
            y: f32,
        }
        let fields = &[
            scalar("x", PodPrimitive::F32),
            scalar("y", PodPrimitive::F32),
        ];
        let bytes = encode_pod(&json!({"x": 10.5, "y": 20.0}), fields).unwrap();
        let back: MouseMove = bytemuck::cast_slice(&bytes)[0];
        assert_eq!(back, MouseMove { x: 10.5, y: 20.0 });
    }

    #[test]
    fn pads_between_u8_and_u32() {
        // #[repr(C)] { a: u8, b: u32 } is 8 bytes: a at 0, 3 bytes of
        // padding, b at 4.
        let fields = &[
            scalar("a", PodPrimitive::U8),
            scalar("b", PodPrimitive::U32),
        ];
        let bytes = encode_pod(&json!({"a": 7, "b": 0x0102_0304u32}), fields).unwrap();
        assert_eq!(bytes, vec![7, 0, 0, 0, 4, 3, 2, 1]);
    }

    #[test]
    fn trailing_padding_for_u64_then_u8() {
        // { a: u64, b: u8 } — 9 bytes of content, rounded to 16 by
        // trailing padding for align-8.
        let fields = &[
            scalar("a", PodPrimitive::U64),
            scalar("b", PodPrimitive::U8),
        ];
        let bytes = encode_pod(&json!({"a": 1u64, "b": 2}), fields).unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(&bytes[0..8], &1u64.to_le_bytes());
        assert_eq!(bytes[8], 2);
        assert_eq!(&bytes[9..16], &[0u8; 7]);
    }

    #[test]
    fn fixed_array_field() {
        let fields = &[array("xs", PodPrimitive::U8, 4)];
        let bytes = encode_pod(&json!({"xs": [1, 2, 3, 4]}), fields).unwrap();
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn missing_field_errors() {
        let fields = &[scalar("code", PodPrimitive::U32)];
        let err = encode_pod(&json!({}), fields).unwrap_err();
        assert!(matches!(err, EncodeError::MissingField(n) if n == "code"));
    }

    #[test]
    fn unexpected_field_errors() {
        let fields = &[scalar("code", PodPrimitive::U32)];
        let err = encode_pod(&json!({"code": 1, "extra": 2}), fields).unwrap_err();
        assert!(matches!(err, EncodeError::UnexpectedField(n) if n == "extra"));
    }

    #[test]
    fn type_mismatch_errors() {
        let fields = &[scalar("code", PodPrimitive::U32)];
        let err = encode_pod(&json!({"code": "not-a-number"}), fields).unwrap_err();
        assert!(matches!(err, EncodeError::TypeMismatch { .. }));
    }

    #[test]
    fn out_of_range_errors() {
        let fields = &[scalar("b", PodPrimitive::U8)];
        let err = encode_pod(&json!({"b": 300}), fields).unwrap_err();
        assert!(matches!(err, EncodeError::OutOfRange { .. }));
    }

    #[test]
    fn array_length_mismatch_errors() {
        let fields = &[array("xs", PodPrimitive::U8, 4)];
        let err = encode_pod(&json!({"xs": [1, 2, 3]}), fields).unwrap_err();
        assert!(matches!(
            err,
            EncodeError::ArrayLengthMismatch {
                expected: 4,
                got: 3,
                ..
            }
        ));
    }

    #[test]
    fn non_object_params_errors() {
        let fields = &[scalar("code", PodPrimitive::U32)];
        let err = encode_pod(&json!([1, 2, 3]), fields).unwrap_err();
        assert!(matches!(err, EncodeError::NotAnObject));
    }

    #[test]
    fn signed_negative_roundtrip() {
        let fields = &[scalar("n", PodPrimitive::I32)];
        let bytes = encode_pod(&json!({"n": -1}), fields).unwrap();
        assert_eq!(bytes, vec![0xff, 0xff, 0xff, 0xff]);
    }

    // ADR-0019 PR 4 — `encode_schema` must produce byte-identical
    // output to `encode_pod` for cast-shaped kinds. PR 5 will retire
    // `encode_pod` once consumers migrate; until then both encoders
    // coexist and a regression in either path is caught here.

    fn schema_scalar(name: &str, ty: Primitive) -> NamedField {
        NamedField {
            name: name.into(),
            ty: SchemaType::Scalar(ty),
        }
    }

    fn schema_struct(fields: Vec<NamedField>) -> SchemaType {
        SchemaType::Struct {
            fields,
            repr_c: true,
        }
    }

    #[test]
    fn schema_unit_encodes_empty_payload() {
        let bytes = encode_schema(&json!({}), &SchemaType::Unit).unwrap();
        assert!(bytes.is_empty());
        let bytes = encode_schema(&json!(null), &SchemaType::Unit).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn schema_unit_rejects_non_empty_object() {
        let err = encode_schema(&json!({"x": 1}), &SchemaType::Unit).unwrap_err();
        assert!(matches!(err, EncodeError::UnexpectedField(_)));
    }

    #[test]
    fn schema_struct_matches_encode_pod_for_key() {
        // Single u32 — the simplest cast struct.
        let pod_bytes = encode_pod(
            &json!({"code": 0xdead_beefu32}),
            &[scalar("code", PodPrimitive::U32)],
        )
        .unwrap();
        let schema_bytes = encode_schema(
            &json!({"code": 0xdead_beefu32}),
            &schema_struct(vec![schema_scalar("code", Primitive::U32)]),
        )
        .unwrap();
        assert_eq!(pod_bytes, schema_bytes);
    }

    #[test]
    fn schema_struct_matches_encode_pod_for_mousemove() {
        // Two f32 — the same shape Pod encoder tests use.
        let pod_bytes = encode_pod(
            &json!({"x": 10.5, "y": 20.0}),
            &[
                scalar("x", PodPrimitive::F32),
                scalar("y", PodPrimitive::F32),
            ],
        )
        .unwrap();
        let schema_bytes = encode_schema(
            &json!({"x": 10.5, "y": 20.0}),
            &schema_struct(vec![
                schema_scalar("x", Primitive::F32),
                schema_scalar("y", Primitive::F32),
            ]),
        )
        .unwrap();
        assert_eq!(pod_bytes, schema_bytes);
    }

    #[test]
    fn schema_struct_pads_between_u8_and_u32_like_pod() {
        // The padding-rule test from `encode_pod` ported to schema —
        // alignment behavior must match for binary compatibility.
        let pod_bytes = encode_pod(
            &json!({"a": 7, "b": 0x0102_0304u32}),
            &[
                scalar("a", PodPrimitive::U8),
                scalar("b", PodPrimitive::U32),
            ],
        )
        .unwrap();
        let schema_bytes = encode_schema(
            &json!({"a": 7, "b": 0x0102_0304u32}),
            &schema_struct(vec![
                schema_scalar("a", Primitive::U8),
                schema_scalar("b", Primitive::U32),
            ]),
        )
        .unwrap();
        assert_eq!(pod_bytes, schema_bytes);
        assert_eq!(schema_bytes, vec![7, 0, 0, 0, 4, 3, 2, 1]);
    }

    #[test]
    fn schema_nested_struct_matches_drawtriangle_layout() {
        // DrawTriangle's shape: { verts: [Vertex; 3] } where Vertex is
        // 5 f32s. Cast wire format = 60 bytes, no internal padding.
        // Pod descriptors couldn't model nested structs so this had to
        // be Opaque before — proving the schema path matches the cast
        // layout end-to-end is the key property of PR 4.
        let vertex = SchemaType::Struct {
            repr_c: true,
            fields: vec![
                schema_scalar("x", Primitive::F32),
                schema_scalar("y", Primitive::F32),
                schema_scalar("r", Primitive::F32),
                schema_scalar("g", Primitive::F32),
                schema_scalar("b", Primitive::F32),
            ],
        };
        let triangle = schema_struct(vec![NamedField {
            name: "verts".into(),
            ty: SchemaType::Array {
                element: Box::new(vertex),
                len: 3,
            },
        }]);
        let v = json!({"x": 0.0, "y": 0.5, "r": 1.0, "g": 0.0, "b": 0.0});
        let params = json!({"verts": [v, v, v]});
        let bytes = encode_schema(&params, &triangle).unwrap();
        assert_eq!(bytes.len(), 60);

        // Roundtrip via bytemuck — same proof shape as the Pod tests.
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, Debug, PartialEq)]
        struct Vertex {
            x: f32,
            y: f32,
            r: f32,
            g: f32,
            b: f32,
        }
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, Debug, PartialEq)]
        struct DrawTriangle {
            verts: [Vertex; 3],
        }
        let back: DrawTriangle = bytemuck::cast_slice(&bytes)[0];
        let v_struct = Vertex {
            x: 0.0,
            y: 0.5,
            r: 1.0,
            g: 0.0,
            b: 0.0,
        };
        assert_eq!(
            back,
            DrawTriangle {
                verts: [v_struct, v_struct, v_struct]
            }
        );
    }

    #[test]
    fn schema_struct_rejects_unexpected_field() {
        let schema = schema_struct(vec![schema_scalar("code", Primitive::U32)]);
        let err = encode_schema(&json!({"code": 1, "extra": 2}), &schema).unwrap_err();
        assert!(matches!(err, EncodeError::UnexpectedField(n) if n == "extra"));
    }

    #[test]
    fn schema_struct_rejects_missing_field() {
        let schema = schema_struct(vec![schema_scalar("code", Primitive::U32)]);
        let err = encode_schema(&json!({}), &schema).unwrap_err();
        assert!(matches!(err, EncodeError::MissingField(n) if n == "code"));
    }

    // ADR-0019 PR 5 — postcard path. Each test asserts that
    // `encode_schema` produces byte-identical output to
    // `postcard::to_allocvec` on an equivalent typed value. That's the
    // load-bearing property: if these match, the substrate decode
    // (via `postcard::from_bytes`) sees the same value the agent sent.

    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct PostcardString {
        body: String,
    }

    #[derive(Serialize, Deserialize)]
    struct PostcardBytes {
        blob: Vec<u8>,
    }

    #[derive(Serialize, Deserialize)]
    struct PostcardOption {
        name: Option<String>,
    }

    #[derive(Serialize, Deserialize)]
    struct PostcardVec {
        tags: Vec<String>,
    }

    #[derive(Serialize, Deserialize)]
    struct Inner {
        seq: u32,
    }

    #[derive(Serialize, Deserialize)]
    struct PostcardNested {
        items: Vec<Inner>,
    }

    #[derive(Serialize, Deserialize)]
    enum SimpleSum {
        Pending,
        Ok(u64),
        Err { reason: String },
    }

    fn pc_string_schema() -> SchemaType {
        SchemaType::Struct {
            repr_c: false,
            fields: vec![NamedField {
                name: "body".into(),
                ty: SchemaType::String,
            }],
        }
    }

    #[test]
    fn postcard_string_field_matches_serde() {
        let value = PostcardString {
            body: "hello world".into(),
        };
        let expected = postcard::to_allocvec(&value).unwrap();
        let bytes = encode_schema(&json!({"body": "hello world"}), &pc_string_schema()).unwrap();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn postcard_string_decodes_back() {
        let bytes = encode_schema(&json!({"body": "round-trip"}), &pc_string_schema()).unwrap();
        let back: PostcardString = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.body, "round-trip");
    }

    #[test]
    fn postcard_bytes_field_matches_serde() {
        let value = PostcardBytes {
            blob: vec![1, 2, 3, 4, 5],
        };
        let expected = postcard::to_allocvec(&value).unwrap();
        let schema = SchemaType::Struct {
            repr_c: false,
            fields: vec![NamedField {
                name: "blob".into(),
                ty: SchemaType::Bytes,
            }],
        };
        let bytes = encode_schema(&json!({"blob": [1, 2, 3, 4, 5]}), &schema).unwrap();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn postcard_option_some_and_none() {
        let schema = SchemaType::Struct {
            repr_c: false,
            fields: vec![NamedField {
                name: "name".into(),
                ty: SchemaType::Option(Box::new(SchemaType::String)),
            }],
        };
        let some = PostcardOption {
            name: Some("Aether".into()),
        };
        let some_bytes = encode_schema(&json!({"name": "Aether"}), &schema).unwrap();
        assert_eq!(some_bytes, postcard::to_allocvec(&some).unwrap());

        let none = PostcardOption { name: None };
        let none_bytes = encode_schema(&json!({"name": null}), &schema).unwrap();
        assert_eq!(none_bytes, postcard::to_allocvec(&none).unwrap());
    }

    #[test]
    fn postcard_vec_of_strings_matches_serde() {
        let value = PostcardVec {
            tags: vec!["alpha".into(), "beta".into(), "gamma".into()],
        };
        let expected = postcard::to_allocvec(&value).unwrap();
        let schema = SchemaType::Struct {
            repr_c: false,
            fields: vec![NamedField {
                name: "tags".into(),
                ty: SchemaType::Vec(Box::new(SchemaType::String)),
            }],
        };
        let bytes = encode_schema(&json!({"tags": ["alpha", "beta", "gamma"]}), &schema).unwrap();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn postcard_vec_of_nested_structs_matches_serde() {
        let value = PostcardNested {
            items: vec![Inner { seq: 1 }, Inner { seq: 256 }, Inner { seq: 0xDEAD }],
        };
        let expected = postcard::to_allocvec(&value).unwrap();
        let inner_schema = SchemaType::Struct {
            repr_c: false,
            fields: vec![NamedField {
                name: "seq".into(),
                ty: SchemaType::Scalar(Primitive::U32),
            }],
        };
        let schema = SchemaType::Struct {
            repr_c: false,
            fields: vec![NamedField {
                name: "items".into(),
                ty: SchemaType::Vec(Box::new(inner_schema)),
            }],
        };
        let bytes = encode_schema(
            &json!({"items": [{"seq": 1}, {"seq": 256}, {"seq": 0xDEAD}]}),
            &schema,
        )
        .unwrap();
        assert_eq!(bytes, expected);
    }

    fn sum_schema() -> SchemaType {
        SchemaType::Enum {
            variants: vec![
                EnumVariant::Unit {
                    name: "Pending".into(),
                    discriminant: 0,
                },
                EnumVariant::Tuple {
                    name: "Ok".into(),
                    discriminant: 1,
                    fields: vec![SchemaType::Scalar(Primitive::U64)],
                },
                EnumVariant::Struct {
                    name: "Err".into(),
                    discriminant: 2,
                    fields: vec![NamedField {
                        name: "reason".into(),
                        ty: SchemaType::String,
                    }],
                },
            ],
        }
    }

    #[test]
    fn postcard_enum_unit_variant_as_string_tag() {
        // Unit variant accepts the bare-string form `"Pending"`.
        let bytes = encode_schema(&json!("Pending"), &sum_schema()).unwrap();
        assert_eq!(bytes, postcard::to_allocvec(&SimpleSum::Pending).unwrap());
    }

    #[test]
    fn postcard_enum_tuple_variant_with_unwrapped_body() {
        // Single-element tuple variants accept either `{"Ok": 42}` or
        // `{"Ok": [42]}`. Cover the unwrapped-body form here.
        let bytes = encode_schema(&json!({"Ok": 42u64}), &sum_schema()).unwrap();
        assert_eq!(bytes, postcard::to_allocvec(&SimpleSum::Ok(42)).unwrap());
    }

    #[test]
    fn postcard_enum_struct_variant() {
        let bytes =
            encode_schema(&json!({"Err": {"reason": "kind conflict"}}), &sum_schema()).unwrap();
        let expected = postcard::to_allocvec(&SimpleSum::Err {
            reason: "kind conflict".into(),
        })
        .unwrap();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn postcard_enum_unknown_tag_errors() {
        let err = encode_schema(&json!("Nope"), &sum_schema()).unwrap_err();
        assert!(matches!(err, EncodeError::TypeMismatch { .. }));
    }

    #[test]
    fn postcard_string_rejects_non_string() {
        let err = encode_schema(&json!({"body": 7}), &pc_string_schema()).unwrap_err();
        assert!(matches!(err, EncodeError::TypeMismatch { .. }));
    }

    #[test]
    fn postcard_struct_rejects_unexpected_field() {
        let err = encode_schema(&json!({"body": "ok", "extra": "nope"}), &pc_string_schema())
            .unwrap_err();
        assert!(matches!(err, EncodeError::UnexpectedField(_)));
    }

    #[test]
    fn varint_matches_postcard_for_boundaries() {
        // 0, 127, 128, 16383, 16384 — each crosses a varint byte
        // boundary and is the most likely place for an off-by-one.
        for n in [0u64, 127, 128, 16383, 16384, u32::MAX as u64, u64::MAX] {
            let mut ours = Vec::new();
            write_varint_u64(&mut ours, n);
            let theirs = postcard::to_allocvec(&n).unwrap();
            assert_eq!(ours, theirs, "varint mismatch for {n}");
        }
    }

    #[test]
    fn zigzag_matches_postcard_for_signed() {
        for n in [0i64, -1, 1, -128, 127, i32::MIN as i64, i32::MAX as i64] {
            let mut ours = Vec::new();
            write_varint_u64(&mut ours, zigzag_i64(n));
            let theirs = postcard::to_allocvec(&n).unwrap();
            assert_eq!(ours, theirs, "zigzag mismatch for {n}");
        }
    }
}
