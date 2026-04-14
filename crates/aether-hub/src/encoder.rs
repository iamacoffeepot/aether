// POD encoder: serde_json params + KindDescriptor field list → bytes
// matching the Rust `#[repr(C)]` layout the engine expects.
//
// Pure function; no hub state, no async. PR 5 wires it into the MCP
// `send_mail` tool; PR 4 ships the function standalone so the logic
// can be exhaustively tested before it's reachable from a tool call.
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
// Nested structs and unions are out of scope for V0 — the descriptor
// format doesn't model them (per ADR-0007). Kinds that need them use
// `KindEncoding::Opaque` and bypass this encoder.

use std::fmt;

use aether_hub_protocol::{PodField, PodFieldType, PodPrimitive};
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
}
