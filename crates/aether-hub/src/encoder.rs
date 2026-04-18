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
// of `encode_pod` for `KindEncoding::Schema(...)` kinds. It currently
// covers the cast subset (`Unit`, top-level `Struct { repr_c: true }`
// containing scalars/arrays/nested cast structs) — same wire bytes as
// `encode_pod`. Postcard-shaped schemas (strings/vecs/options/enums)
// land in PR 5; until then they error out with a clear message.

use std::fmt;

use aether_hub_protocol::{
    NamedField, PodField, PodFieldType, PodPrimitive, Primitive, SchemaType,
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
    /// A `SchemaType` arm the hub can't yet encode. ADR-0019 PR 4 only
    /// covers cast-shaped schemas; postcard-shaped ones (strings, vecs,
    /// options, enums) land in PR 5. The variant carries a description
    /// of the offending shape so the agent error is actionable.
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
                write!(
                    f,
                    "schema arm not yet supported by hub encoder: {shape} (ADR-0019 PR 5)"
                )
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

/// ADR-0019: encode `params` against a `SchemaType` descriptor. Cast
/// subset only in this PR — top-level `Unit`, top-level `Struct {
/// repr_c: true }` containing scalars, fixed arrays, and nested
/// `Struct { repr_c: true }`. Wire bytes match what `encode_pod`
/// produces for the same logical shape, which is what the substrate
/// already decodes via `bytemuck::cast` on the receive side.
///
/// Postcard-shaped schemas (`String`, `Bytes`, `Vec`, `Option`,
/// `Enum`, or any `Struct { repr_c: false }`) return
/// `EncodeError::UnsupportedSchema` until PR 5 wires the postcard
/// path through this function.
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
        SchemaType::Struct { repr_c: false, .. } => {
            Err(EncodeError::UnsupportedSchema("Struct { repr_c: false }"))
        }
        SchemaType::Bool => Err(EncodeError::UnsupportedSchema("Bool top-level")),
        SchemaType::Scalar(_) => Err(EncodeError::UnsupportedSchema("Scalar top-level")),
        SchemaType::String => Err(EncodeError::UnsupportedSchema("String")),
        SchemaType::Bytes => Err(EncodeError::UnsupportedSchema("Bytes")),
        SchemaType::Option(_) => Err(EncodeError::UnsupportedSchema("Option")),
        SchemaType::Vec(_) => Err(EncodeError::UnsupportedSchema("Vec")),
        SchemaType::Array { .. } => Err(EncodeError::UnsupportedSchema("Array top-level")),
        SchemaType::Enum { .. } => Err(EncodeError::UnsupportedSchema("Enum")),
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
    fn schema_postcard_struct_errors_until_pr5() {
        let schema = SchemaType::Struct {
            fields: vec![NamedField {
                name: "label".into(),
                ty: SchemaType::String,
            }],
            repr_c: false,
        };
        let err = encode_schema(&json!({"label": "x"}), &schema).unwrap_err();
        assert!(matches!(err, EncodeError::UnsupportedSchema(_)));
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
}
