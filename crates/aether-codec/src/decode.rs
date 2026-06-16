// Wire-decode: bytes laid out per `SchemaType` → serde_json. The
// narrowing casts (`u64 → u32`, varint slot to signed via
// `cast_possible_wrap`) are the load-bearing inverse of the encode
// path; `From::from` / `try_into` would obscure the byte-layout
// contract this function implements.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

// `decode_schema`: wire bytes + `SchemaType` descriptor → serde_json
// value the agent can read directly. Mirror of `encoder::encode_schema`
// — same two paths, picked the same way:
//
// 1. Cast-shaped (`Struct { repr_c: true }` and the recursive tree
//    under it): walk `#[repr(C)]` byte layout, lift each scalar / fixed
//    array into JSON. Encoder pads to alignment between fields and
//    rounds total size to the largest field alignment; the decoder does
//    the same skips.
//
// 2. Postcard (everything else): consume the postcard 1.x wire format
//    directly — varints, zigzag, length-prefixed strings/vecs/bytes,
//    externally-tagged enums.
//
// We decode the bytes directly rather than going through serde's
// deserializer because the descriptor is structural (not a typed
// schema), and the encoder writes bytes directly for the same reason.
// Round-trip tests against the encoder pin the wire format from both
// sides.

use std::fmt;

use aether_data::{EnumVariant, NamedField, Primitive, SchemaType};
use serde_json::{Map, Value};

use crate::cast::{align_of_primitive, non_cast_variant_error};
use aether_data::tagged_id;
use std::error;
use std::str;

#[derive(Debug)]
pub enum DecodeError {
    Truncated {
        path: String,
        needed: usize,
        had: usize,
    },
    TrailingBytes {
        path: String,
        remaining: usize,
    },
    InvalidBool {
        path: String,
        byte: u8,
    },
    InvalidUtf8 {
        path: String,
    },
    VarintOverflow {
        path: String,
    },
    UnknownEnumDiscriminant {
        path: String,
        discriminant: u32,
    },
    /// The decode produced more `Value` nodes than the input length
    /// justifies (`VALUE_BUDGET_BASE + input_len * VALUES_PER_INPUT_BYTE`).
    /// Guards the zero-wire-byte-element collection class (`Vec<Unit>`,
    /// `Vec<Struct {}>`) whose decode loop allocates a `Value` per
    /// iteration without consuming input — the pre-allocation clamp
    /// alone can't bound it. Same altitude as `frame.rs`'s
    /// `MAX_FRAME_SIZE`: a length prefix must not drive a reader into an
    /// unbounded allocation.
    ValueBudgetExceeded {
        path: String,
        budget: usize,
    },
    /// Schema arm the hub decoder can't handle in this position. Mirror
    /// of the encoder's same variant — fires for non-cast leaf types
    /// inside a cast-shaped parent.
    UnsupportedSchema(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { path, needed, had } => {
                write!(f, "truncated at {path}: needed {needed} bytes, had {had}")
            }
            Self::TrailingBytes { path, remaining } => write!(
                f,
                "trailing bytes after decoding {path}: {remaining} unread"
            ),
            Self::InvalidBool { path, byte } => {
                write!(f, "invalid bool at {path}: 0x{byte:02x} not 0 or 1")
            }
            Self::InvalidUtf8 { path } => write!(f, "invalid utf-8 in string at {path}"),
            Self::VarintOverflow { path } => {
                write!(f, "varint at {path} exceeds 10 bytes (overflow)")
            }
            Self::ValueBudgetExceeded { path, budget } => write!(
                f,
                "decode value budget exceeded at {path}: more than {budget} values for the input length"
            ),
            Self::UnknownEnumDiscriminant { path, discriminant } => write!(
                f,
                "enum at {path} has no variant for discriminant {discriminant}"
            ),
            Self::UnsupportedSchema(shape) => {
                write!(f, "schema arm not supported by hub decoder: {shape}")
            }
        }
    }
}

impl error::Error for DecodeError {}

/// Decode-side allocation budget, in the spirit of `frame.rs`'s
/// `MAX_FRAME_SIZE`: a wire-decoded length must never drive the decoder
/// into an unbounded allocation. Every postcard node charges one value
/// against a per-decode budget sized from the input length, so a crafted
/// length — or a zero-wire-byte-element collection (`Unit`, field-less
/// `Struct`) whose decode loop allocates per iteration without consuming
/// input — can't expand into more values than the bytes justify.
///
/// The budget is `VALUE_BUDGET_BASE + input_len * VALUES_PER_INPUT_BYTE`.
/// Every node except the zero-wire-byte class consumes at least one input
/// byte, so valid decodes sit near one value per byte; the linear term
/// keeps frame-scale payloads decodable (a `Bytes` field decodes one
/// value per byte, so a default-config 64 MiB frame legitimately produces
/// tens of millions of values), and the base term absorbs small
/// zero-byte-element collections (the proptest generator's depth-≤4 /
/// width-≤4 trees peak at a few hundred values). What it rejects is the
/// decompression-bomb class: a decoded value count unjustified by the
/// bytes actually sent. A global budget is the only bound that composes —
/// per-arm caps multiply under nesting (`Vec<Vec<Unit>>` turns a per-arm
/// cap of C into `input_bytes × C` values).
const VALUE_BUDGET_BASE: usize = 4096;
const VALUES_PER_INPUT_BYTE: usize = 4;

/// ADR-0020: decode `bytes` against a `SchemaType` descriptor into a
/// JSON value symmetric to what `encode_schema` would accept.
/// Dispatches on the schema's wire shape (same split as the encoder):
///
/// - `Unit` → `null` (empty payload).
/// - `Struct { repr_c: true }` (and the recursive cast-shaped tree
///   under it) → walk the `#[repr(C)]` byte layout.
/// - Everything else → consume postcard 1.x wire format.
///
/// Trailing bytes are an error (the encoder writes exactly the right
/// number of bytes; extras mean schema/payload drift the agent should
/// see).
pub fn decode_schema(bytes: &[u8], schema: &SchemaType) -> Result<Value, DecodeError> {
    let mut cur = Cursor::new(bytes);
    let value = decode_value(&mut cur, schema, "$")?;
    if cur.remaining() != 0 {
        return Err(DecodeError::TrailingBytes {
            path: "$".into(),
            remaining: cur.remaining(),
        });
    }
    Ok(value)
}

fn decode_value(
    cur: &mut Cursor<'_>,
    schema: &SchemaType,
    path: &str,
) -> Result<Value, DecodeError> {
    match schema {
        SchemaType::Unit => Ok(Value::Null),
        SchemaType::Struct {
            fields,
            repr_c: true,
        } => {
            let obj = decode_cast_struct(cur, fields, path)?;
            let max_align = struct_alignment(fields)?;
            cur.skip_pad_to(max_align);
            Ok(Value::Object(obj))
        }
        _ => decode_postcard(cur, schema, path),
    }
}

fn decode_cast_struct(
    cur: &mut Cursor<'_>,
    fields: &[NamedField],
    path: &str,
) -> Result<Map<String, Value>, DecodeError> {
    let mut out = Map::with_capacity(fields.len());
    for field in fields {
        let field_path = format!("{path}.{}", field.name);
        let value = decode_cast_field(cur, &field.ty, &field_path)?;
        out.insert(field.name.to_string(), value);
    }
    Ok(out)
}

fn decode_cast_field(
    cur: &mut Cursor<'_>,
    ty: &SchemaType,
    path: &str,
) -> Result<Value, DecodeError> {
    // Non-cast variants share the same error message across encode +
    // decode; `cast::non_cast_variant_error` owns the classification
    // (and its own exhaustiveness check forces new SchemaType variants
    // to declare which side they fall on).
    if let Some(msg) = non_cast_variant_error(ty) {
        return Err(DecodeError::UnsupportedSchema(msg));
    }
    match ty {
        SchemaType::Scalar(p) => {
            let a = align_of_primitive(*p);
            cur.skip_pad_to(a);
            read_primitive_cast(cur, *p, path)
        }
        SchemaType::Array { element, len } => {
            let elem_align = alignment_of_schema(element)?;
            cur.skip_pad_to(elem_align);
            let mut arr = Vec::with_capacity(*len as usize);
            for i in 0..*len {
                let elem_path = format!("{path}[{i}]");
                arr.push(decode_cast_field(cur, element, &elem_path)?);
            }
            Ok(Value::Array(arr))
        }
        SchemaType::Struct {
            fields,
            repr_c: true,
        } => {
            let nested_align = alignment_of_schema(ty)?;
            cur.skip_pad_to(nested_align);
            let obj = decode_cast_struct(cur, fields, path)?;
            let inner_max = struct_alignment(fields)?;
            cur.skip_pad_to(inner_max);
            Ok(Value::Object(obj))
        }
        SchemaType::Struct { repr_c: false, .. } => Err(DecodeError::UnsupportedSchema(
            "Struct { repr_c: false } in cast-shaped parent",
        )),
        SchemaType::TypeId(type_id) => {
            // ADR-0065: typed-id inside cast-shape parent. 8 bytes
            // LE, 8-byte align — same as a `u64`.
            cur.skip_pad_to(8);
            let id = u64::from_le_bytes(cur.take::<8>(path)?);
            Ok(render_type_id_value(id, *type_id, path)?)
        }
        _ => unreachable!(
            "non-cast SchemaType variants returned early via non_cast_variant_error; \
             a new cast-eligible variant must be classified there and added here"
        ),
    }
}

/// u64 → JSON helper for `SchemaType::TypeId(type_id)`. Emits the
/// ADR-0064 tagged string form when the id's tag bits are valid;
/// falls back to a JSON number for the reserved-tag sentinels (e.g.
/// `MailboxId::NONE = 0`) so the codec doesn't error on a sentinel
/// payload. Errors with `UnsupportedSchema` if the schema's
/// `type_id` doesn't correspond to a typed-id newtype the codec
/// knows how to translate.
fn render_type_id_value(id: u64, type_id: u64, _path: &str) -> Result<Value, DecodeError> {
    let _expected = aether_data::tag_for_type_id(type_id)
        .ok_or(DecodeError::UnsupportedSchema("unknown TypeId in schema"))?;
    Ok(tagged_id::encode(id).map_or_else(|| Value::from(id), Value::String))
}

fn read_primitive_cast(
    cur: &mut Cursor<'_>,
    p: Primitive,
    path: &str,
) -> Result<Value, DecodeError> {
    match p {
        Primitive::U8 => Ok(Value::from(u8::from_le_bytes(cur.take::<1>(path)?))),
        Primitive::U16 => Ok(Value::from(u16::from_le_bytes(cur.take::<2>(path)?))),
        Primitive::U32 => Ok(Value::from(u32::from_le_bytes(cur.take::<4>(path)?))),
        Primitive::U64 => Ok(Value::from(u64::from_le_bytes(cur.take::<8>(path)?))),
        Primitive::I8 => Ok(Value::from(i8::from_le_bytes(cur.take::<1>(path)?))),
        Primitive::I16 => Ok(Value::from(i16::from_le_bytes(cur.take::<2>(path)?))),
        Primitive::I32 => Ok(Value::from(i32::from_le_bytes(cur.take::<4>(path)?))),
        Primitive::I64 => Ok(Value::from(i64::from_le_bytes(cur.take::<8>(path)?))),
        Primitive::F32 => Ok(json_f64(f64::from(f32::from_le_bytes(
            cur.take::<4>(path)?,
        )))),
        Primitive::F64 => Ok(json_f64(f64::from_le_bytes(cur.take::<8>(path)?))),
    }
}

fn struct_alignment(fields: &[NamedField]) -> Result<usize, DecodeError> {
    let mut a = 1usize;
    for f in fields {
        a = a.max(alignment_of_schema(&f.ty)?);
    }
    Ok(a)
}

fn alignment_of_schema(ty: &SchemaType) -> Result<usize, DecodeError> {
    match ty {
        SchemaType::Scalar(p) => Ok(align_of_primitive(*p)),
        // ADR-0065: typed ids are u64-shaped — 8 bytes, 8-byte align.
        SchemaType::TypeId(_) => Ok(8),
        SchemaType::Array { element, .. } => alignment_of_schema(element),
        SchemaType::Struct {
            fields,
            repr_c: true,
        } => struct_alignment(fields),
        _ => Err(DecodeError::UnsupportedSchema(
            "alignment query on non-cast schema",
        )),
    }
}

// Schema-driven postcard decoder: one match arm per `SchemaType`
// variant. Each arm is short but the arm count adds up — extracting
// per-type helpers obscures the schema → wire mapping that's the
// purpose of this fn.
#[allow(clippy::too_many_lines)]
fn decode_postcard(
    cur: &mut Cursor<'_>,
    schema: &SchemaType,
    path: &str,
) -> Result<Value, DecodeError> {
    // Every postcard node charges exactly once — collection elements,
    // struct fields, enum bodies — including through recursion, so the
    // decode-wide budget bounds the zero-wire-byte-element class whose
    // loop allocates without consuming input.
    cur.charge_value(path)?;
    match schema {
        SchemaType::Unit => Ok(Value::Null),
        SchemaType::Bool => {
            let [b] = cur.take::<1>(path)?;
            match b {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                _ => Err(DecodeError::InvalidBool {
                    path: path.into(),
                    byte: b,
                }),
            }
        }
        SchemaType::Scalar(p) => read_primitive_postcard(cur, *p, path),
        SchemaType::String => {
            let len = read_varint_u64(cur, path)? as usize;
            let bytes = cur.take_slice(len, path)?;
            let s = str::from_utf8(bytes)
                .map_err(|_| DecodeError::InvalidUtf8 { path: path.into() })?;
            Ok(Value::String(s.into()))
        }
        SchemaType::Bytes => {
            let len = read_varint_u64(cur, path)? as usize;
            let bytes = cur.take_slice(len, path)?;
            // Mirror encoder input shape: array of byte values.
            let arr = bytes.iter().map(|b| Value::from(*b)).collect();
            Ok(Value::Array(arr))
        }
        SchemaType::Option(inner) => {
            let [tag] = cur.take::<1>(path)?;
            match tag {
                0 => Ok(Value::Null),
                1 => decode_postcard(cur, inner, path),
                _ => Err(DecodeError::InvalidBool {
                    path: path.into(),
                    byte: tag,
                }),
            }
        }
        SchemaType::Vec(inner) => {
            let len = read_varint_u64(cur, path)? as usize;
            // Clamp the pre-allocation against the bytes that remain: a
            // varint-encoded element occupies ≥ 1 byte, so a `len` past
            // `remaining` can't be valid non-degenerate input. Zero-byte
            // elements start small and grow by push; the decode-wide
            // budget bounds that loop.
            let mut arr = Vec::with_capacity(len.min(cur.remaining()));
            for i in 0..len {
                let elem_path = format!("{path}[{i}]");
                arr.push(decode_postcard(cur, inner, &elem_path)?);
            }
            Ok(Value::Array(arr))
        }
        SchemaType::Array { element, len } => {
            let mut arr = Vec::with_capacity(*len as usize);
            for i in 0..*len {
                let elem_path = format!("{path}[{i}]");
                arr.push(decode_postcard(cur, element, &elem_path)?);
            }
            Ok(Value::Array(arr))
        }
        SchemaType::Struct { fields, .. } => {
            // Postcard struct: concatenated field bytes in declaration
            // order.
            let mut obj = Map::with_capacity(fields.len());
            for field in fields.iter() {
                let field_path = format!("{path}.{}", field.name);
                let value = decode_postcard(cur, &field.ty, &field_path)?;
                obj.insert(field.name.to_string(), value);
            }
            Ok(Value::Object(obj))
        }
        SchemaType::Enum { variants } => {
            let disc = read_varint_u64(cur, path)? as u32;
            let variant = variants
                .iter()
                .find(|v| v.discriminant() == disc)
                .ok_or_else(|| DecodeError::UnknownEnumDiscriminant {
                    path: path.into(),
                    discriminant: disc,
                })?;
            decode_enum_body(cur, variant, path)
        }
        SchemaType::Map {
            key: key_schema,
            value: value_schema,
        } => {
            // Issue #232 + proto3-style JSON mapping. Wire is
            // postcard's `BTreeMap<K, V>` shape — varint(len) followed
            // by `(k, v)` pairs in key-sorted order. We emit a JSON
            // object with the proto3 stringify rule: integer keys as
            // decimal-string keys, bool keys as `"true"`/`"false"`,
            // string keys identity. Order in the emitted object isn't
            // load-bearing for decoders that compare by value.
            let len = read_varint_u64(cur, path)? as usize;
            // Same clamp as the `Vec` arm: a `(k, v)` pair occupies ≥ 1
            // byte, so cap the pre-allocation at the bytes remaining.
            let mut obj = Map::with_capacity(len.min(cur.remaining()));
            for i in 0..len {
                let entry_path = format!("{path}[{i}]");
                let key_value = decode_postcard(cur, key_schema, &entry_path)?;
                let val_value = decode_postcard(cur, value_schema, &entry_path)?;
                let key_string = render_map_key(&key_value, key_schema, &entry_path)?;
                obj.insert(key_string, val_value);
            }
            Ok(Value::Object(obj))
        }
        SchemaType::Ref(inner) => {
            // ADR-0045 typed handle, inline arm revised by ADR-0100.
            // Wire matches the postcard enum encoding: discriminant
            // varint, then either the inline body (Inline = 0) or two
            // varints id + kind_id (Handle = 1). The inline body is the
            // inner kind's own codec image, length-prefixed — read the
            // length, slice it off, and decode it with the same
            // cast-or-postcard dispatch as a top-level kind. Render as
            // externally-tagged JSON to match the encoder's input shape.
            let disc = read_varint_u64(cur, path)? as u32;
            match disc {
                0 => {
                    let len = read_varint_u64(cur, path)? as usize;
                    let body = cur.take_slice(len, path)?;
                    let inner_value = decode_schema(body, inner)?;
                    let mut obj = Map::with_capacity(1);
                    obj.insert("Inline".into(), inner_value);
                    Ok(Value::Object(obj))
                }
                1 => {
                    let id = read_varint_u64(cur, &format!("{path}.id"))?;
                    let kind_id = read_varint_u64(cur, &format!("{path}.kind_id"))?;
                    let mut handle_obj = Map::with_capacity(2);
                    handle_obj.insert("id".into(), Value::from(id));
                    handle_obj.insert("kind_id".into(), Value::from(kind_id));
                    let mut obj = Map::with_capacity(1);
                    obj.insert("Handle".into(), Value::Object(handle_obj));
                    Ok(Value::Object(obj))
                }
                _ => Err(DecodeError::UnknownEnumDiscriminant {
                    path: path.into(),
                    discriminant: disc,
                }),
            }
        }
        SchemaType::TypeId(type_id) => {
            // ADR-0065 typed id. Wire is a u64 varint; emit the
            // tagged string form (or back-compat number for
            // reserved-tag sentinels).
            let id = read_varint_u64(cur, path)?;
            render_type_id_value(id, *type_id, path)
        }
    }
}

fn read_primitive_postcard(
    cur: &mut Cursor<'_>,
    p: Primitive,
    path: &str,
) -> Result<Value, DecodeError> {
    match p {
        Primitive::U8 => Ok(Value::from(cur.take::<1>(path)?[0])),
        Primitive::U16 => {
            let n = read_varint_u64(cur, path)?;
            Ok(Value::from(n as u16))
        }
        Primitive::U32 => {
            let n = read_varint_u64(cur, path)?;
            Ok(Value::from(n as u32))
        }
        Primitive::U64 => {
            let n = read_varint_u64(cur, path)?;
            Ok(Value::from(n))
        }
        Primitive::I8 => Ok(Value::from(cur.take::<1>(path)?[0] as i8)),
        Primitive::I16 => {
            let n = read_varint_u64(cur, path)?;
            Ok(Value::from(unzigzag(n) as i16))
        }
        Primitive::I32 => {
            let n = read_varint_u64(cur, path)?;
            Ok(Value::from(unzigzag(n) as i32))
        }
        Primitive::I64 => {
            let n = read_varint_u64(cur, path)?;
            Ok(Value::from(unzigzag(n)))
        }
        Primitive::F32 => Ok(json_f64(f64::from(f32::from_le_bytes(
            cur.take::<4>(path)?,
        )))),
        Primitive::F64 => Ok(json_f64(f64::from_le_bytes(cur.take::<8>(path)?))),
    }
}

fn decode_enum_body(
    cur: &mut Cursor<'_>,
    variant: &EnumVariant,
    path: &str,
) -> Result<Value, DecodeError> {
    let name = variant.name().to_owned();
    match variant {
        EnumVariant::Unit { .. } => {
            // Unit variant: bare-string tag, no body. Symmetric to the
            // encoder accepting `"Variant"`.
            Ok(Value::String(name))
        }
        EnumVariant::Tuple { fields, .. } => {
            let body = if fields.len() == 1 {
                let nested_path = format!("{path}::{name}.0");
                decode_postcard(cur, &fields[0], &nested_path)?
            } else {
                let mut arr = Vec::with_capacity(fields.len());
                for (i, ty) in fields.iter().enumerate() {
                    let nested = format!("{path}::{name}.{i}");
                    arr.push(decode_postcard(cur, ty, &nested)?);
                }
                Value::Array(arr)
            };
            let mut obj = Map::with_capacity(1);
            obj.insert(name, body);
            Ok(Value::Object(obj))
        }
        EnumVariant::Struct { fields, .. } => {
            let mut body = Map::with_capacity(fields.len());
            for field in fields.iter() {
                let nested = format!("{path}::{name}.{}", field.name);
                let v = decode_postcard(cur, &field.ty, &nested)?;
                body.insert(field.name.to_string(), v);
            }
            let mut obj = Map::with_capacity(1);
            obj.insert(name, Value::Object(body));
            Ok(Value::Object(obj))
        }
    }
}

/// Stringify a decoded map key into its proto3-JSON form (issue #232).
/// Mirrors the encoder's `parse_map_key`: string identity, integer
/// scalars to decimal digits, bool to `"true"`/`"false"`. Anything else
/// is `UnsupportedSchema` — the `BTreeMap`<K: Ord, V> bound at the Rust
/// layer makes those unreachable, but the codec rejects them defensively
/// in case a descriptor lands here from an external source.
fn render_map_key(
    key_value: &Value,
    key_schema: &SchemaType,
    path: &str,
) -> Result<String, DecodeError> {
    match (key_schema, key_value) {
        (SchemaType::String, Value::String(s)) => Ok(s.clone()),
        (SchemaType::Bool, Value::Bool(b)) => Ok(if *b { "true".into() } else { "false".into() }),
        (SchemaType::Scalar(p), Value::Number(n)) => match p {
            Primitive::U8 | Primitive::U16 | Primitive::U32 | Primitive::U64 => Ok(n
                .as_u64()
                .ok_or(DecodeError::UnsupportedSchema(
                    "decoded unsigned key value out of u64 range",
                ))?
                .to_string()),
            Primitive::I8 | Primitive::I16 | Primitive::I32 | Primitive::I64 => Ok(n
                .as_i64()
                .ok_or(DecodeError::UnsupportedSchema(
                    "decoded signed key value out of i64 range",
                ))?
                .to_string()),
            Primitive::F32 | Primitive::F64 => {
                Err(DecodeError::UnsupportedSchema("float as Map key (no Ord)"))
            }
        },
        _ => {
            let _ = path;
            Err(DecodeError::UnsupportedSchema(
                "Map key must be String, integer scalar, or Bool",
            ))
        }
    }
}

/// Postcard 1.x varint: 7 bits per byte, MSB set means continue. Cap at
/// 10 bytes — anything longer is overflow for u64.
fn read_varint_u64(cur: &mut Cursor<'_>, path: &str) -> Result<u64, DecodeError> {
    let mut n: u64 = 0;
    let mut shift = 0u32;
    for _ in 0..10 {
        let [b] = cur.take::<1>(path)?;
        n |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
    }
    Err(DecodeError::VarintOverflow { path: path.into() })
}

fn unzigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// JSON numbers can't represent NaN/infinity. The encoder accepts
/// arbitrary `f64`s; on decode we coerce non-finite to `null` so the
/// JSON value remains valid. Round-trip semantics: finite floats round
/// trip exactly; NaN/inf bytes decode to null (loud, not silent).
fn json_f64(n: f64) -> Value {
    serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// Remaining value budget for this decode (see `VALUE_BUDGET_BASE`).
    /// Each postcard node decrements it via `charge_value`.
    values_left: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        // Saturating: a `bytes.len()` near `usize::MAX` is not reachable
        // (it's a real slice), but the arithmetic stays defined.
        let values_left =
            VALUE_BUDGET_BASE.saturating_add(bytes.len().saturating_mul(VALUES_PER_INPUT_BYTE));
        Self {
            bytes,
            pos: 0,
            values_left,
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Charge one value against the decode-wide budget. Returns
    /// `ValueBudgetExceeded` once the budget is exhausted, so a decode
    /// can't expand into more `Value` nodes than the input length
    /// justifies — the bound for zero-wire-byte-element collections.
    fn charge_value(&mut self, path: &str) -> Result<(), DecodeError> {
        match self.values_left.checked_sub(1) {
            Some(remaining) => {
                self.values_left = remaining;
                Ok(())
            }
            None => Err(DecodeError::ValueBudgetExceeded {
                path: path.into(),
                budget: VALUE_BUDGET_BASE
                    .saturating_add(self.bytes.len().saturating_mul(VALUES_PER_INPUT_BYTE)),
            }),
        }
    }

    fn take<const N: usize>(&mut self, path: &str) -> Result<[u8; N], DecodeError> {
        if self.remaining() < N {
            return Err(DecodeError::Truncated {
                path: path.into(),
                needed: N,
                had: self.remaining(),
            });
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    fn take_slice(&mut self, n: usize, path: &str) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated {
                path: path.into(),
                needed: n,
                had: self.remaining(),
            });
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Advance past zero-padding so `pos` lands on a multiple of `align`.
    /// Mirror of `encoder::pad_to`. Padding bytes are not validated as
    /// zero — the encoder writes zeros, but a third-party encoder might
    /// not, and the descriptor wins either way.
    fn skip_pad_to(&mut self, align: usize) {
        while !self.pos.is_multiple_of(align) && self.pos < self.bytes.len() {
            self.pos += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_schema;
    use crate::test_fixtures::{cast_struct, pending_ok_err_variants, postcard_struct, scalar};
    use aether_data::SchemaCell;
    use aether_data::tagged_id;
    use serde_json::json;

    /// Local alias preserving the decode-side spelling that the test
    /// bodies below already use.
    fn pc_struct(fields: Vec<NamedField>) -> SchemaType {
        postcard_struct(fields)
    }

    /// Encode → decode → assert equal. The single most load-bearing
    /// invariant: every kind shape the encoder accepts, the decoder
    /// inverts.
    // `value` is owned because the test passes a freshly-built `Value`
    // (e.g. `Value::String("…".to_owned())`) inline at the call site;
    // taking `&Value` would force ad-hoc bindings at every site.
    #[allow(clippy::needless_pass_by_value)]
    fn roundtrip(value: Value, schema: &SchemaType) {
        let bytes = encode_schema(&value, schema)
            .unwrap_or_else(|e| panic!("encode failed for {value:?}: {e}"));
        let back = decode_schema(&bytes, schema)
            .unwrap_or_else(|e| panic!("decode failed for {value:?}: {e}"));
        assert_eq!(back, value, "round-trip mismatch for {value:?}");
    }

    #[test]
    fn unit_decodes_null() {
        let v = decode_schema(&[], &SchemaType::Unit).expect("test setup: decode empty unit");
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn unit_rejects_trailing_bytes() {
        let err = decode_schema(&[1, 2, 3], &SchemaType::Unit)
            .expect_err("trailing bytes after unit must error");
        assert!(matches!(err, DecodeError::TrailingBytes { .. }));
    }

    // Cast-shaped path

    #[test]
    fn cast_single_u32() {
        roundtrip(
            json!({"code": 42u32}),
            &cast_struct(vec![scalar("code", Primitive::U32)]),
        );
    }

    #[test]
    fn cast_two_f32_fields() {
        roundtrip(
            json!({"x": 1.5, "y": -3.25}),
            &cast_struct(vec![
                scalar("x", Primitive::F32),
                scalar("y", Primitive::F32),
            ]),
        );
    }

    #[test]
    fn cast_padding_between_u8_and_u32() {
        roundtrip(
            json!({"a": 7u8, "b": 0x0102_0304u32}),
            &cast_struct(vec![
                scalar("a", Primitive::U8),
                scalar("b", Primitive::U32),
            ]),
        );
    }

    #[test]
    fn cast_trailing_padding_for_u64_then_u8() {
        // Encoder pads to 16 bytes; decoder must skip the trailing 7
        // zeros before checking for trailing bytes.
        roundtrip(
            json!({"a": 1u64, "b": 2u8}),
            &cast_struct(vec![
                scalar("a", Primitive::U64),
                scalar("b", Primitive::U8),
            ]),
        );
    }

    #[test]
    fn cast_fixed_array_field() {
        roundtrip(
            json!({"xs": [1u8, 2, 3, 4]}),
            &cast_struct(vec![NamedField {
                name: "xs".into(),
                ty: SchemaType::Array {
                    element: SchemaCell::owned(SchemaType::Scalar(Primitive::U8)),
                    len: 4,
                },
            }]),
        );
    }

    #[test]
    fn cast_signed_negative_roundtrip() {
        roundtrip(
            json!({"n": -1}),
            &cast_struct(vec![scalar("n", Primitive::I32)]),
        );
    }

    #[test]
    fn cast_nested_struct_drawtriangle_layout() {
        // Mirror of the encoder test by the same name. The DrawTriangle
        // shape is the load-bearing cast-nested case in the codebase.
        let vertex = cast_struct(vec![
            scalar("x", Primitive::F32),
            scalar("y", Primitive::F32),
            scalar("r", Primitive::F32),
            scalar("g", Primitive::F32),
            scalar("b", Primitive::F32),
        ]);
        let triangle = cast_struct(vec![NamedField {
            name: "verts".into(),
            ty: SchemaType::Array {
                element: SchemaCell::owned(vertex),
                len: 3,
            },
        }]);
        let v = json!({"x": 0.0, "y": 0.5, "r": 1.0, "g": 0.0, "b": 0.0});
        roundtrip(json!({"verts": [v.clone(), v.clone(), v]}), &triangle);
    }

    #[test]
    fn cast_truncated_payload_errors() {
        // 4-byte u32 expected, only 2 bytes provided.
        let schema = cast_struct(vec![scalar("code", Primitive::U32)]);
        let err = decode_schema(&[1, 2], &schema).expect_err("truncated u32 payload must error");
        assert!(matches!(err, DecodeError::Truncated { .. }));
    }

    // Postcard path — primitives

    #[test]
    fn postcard_bool_field() {
        roundtrip(
            json!({"flag": true}),
            &pc_struct(vec![NamedField {
                name: "flag".into(),
                ty: SchemaType::Bool,
            }]),
        );
        roundtrip(
            json!({"flag": false}),
            &pc_struct(vec![NamedField {
                name: "flag".into(),
                ty: SchemaType::Bool,
            }]),
        );
    }

    #[test]
    fn postcard_invalid_bool_byte_errors() {
        let schema = pc_struct(vec![NamedField {
            name: "flag".into(),
            ty: SchemaType::Bool,
        }]);
        let err = decode_schema(&[2], &schema).expect_err("non-0/1 bool byte must error");
        assert!(matches!(err, DecodeError::InvalidBool { .. }));
    }

    #[test]
    fn postcard_string_field() {
        roundtrip(
            json!({"body": "hello world"}),
            &pc_struct(vec![NamedField {
                name: "body".into(),
                ty: SchemaType::String,
            }]),
        );
    }

    #[test]
    fn postcard_string_invalid_utf8_errors() {
        let schema = pc_struct(vec![NamedField {
            name: "body".into(),
            ty: SchemaType::String,
        }]);
        // varint length 2, then two invalid utf-8 bytes.
        let err = decode_schema(&[2, 0xff, 0xfe], &schema)
            .expect_err("invalid utf-8 string body must error");
        assert!(matches!(err, DecodeError::InvalidUtf8 { .. }));
    }

    #[test]
    fn postcard_bytes_field() {
        roundtrip(
            json!({"blob": [1u8, 2, 3, 4, 5]}),
            &pc_struct(vec![NamedField {
                name: "blob".into(),
                ty: SchemaType::Bytes,
            }]),
        );
    }

    #[test]
    fn postcard_option_some_and_none() {
        let schema = pc_struct(vec![NamedField {
            name: "name".into(),
            ty: SchemaType::Option(SchemaCell::owned(SchemaType::String)),
        }]);
        roundtrip(json!({"name": "Aether"}), &schema);
        roundtrip(json!({"name": null}), &schema);
    }

    #[test]
    fn postcard_vec_of_strings() {
        let schema = pc_struct(vec![NamedField {
            name: "tags".into(),
            ty: SchemaType::Vec(SchemaCell::owned(SchemaType::String)),
        }]);
        roundtrip(json!({"tags": ["alpha", "beta", "gamma"]}), &schema);
    }

    #[test]
    fn postcard_vec_of_nested_structs() {
        let inner = pc_struct(vec![scalar("seq", Primitive::U32)]);
        let schema = pc_struct(vec![NamedField {
            name: "items".into(),
            ty: SchemaType::Vec(SchemaCell::owned(inner)),
        }]);
        roundtrip(
            json!({"items": [{"seq": 1u32}, {"seq": 256u32}, {"seq": 0xDEADu32}]}),
            &schema,
        );
    }

    fn sum_schema() -> SchemaType {
        SchemaType::Enum {
            variants: pending_ok_err_variants().into(),
        }
    }

    #[test]
    fn postcard_enum_unit_variant_decodes_as_string_tag() {
        roundtrip(json!("Pending"), &sum_schema());
    }

    #[test]
    fn postcard_enum_tuple_single_field_decodes_unwrapped() {
        // Encoder accepts both `{"Ok": 42}` and `{"Ok": [42]}` for
        // single-field tuples; decoder normalizes to the unwrapped
        // form so round-trip from `{"Ok": 42}` is byte-equal.
        roundtrip(json!({"Ok": 42u64}), &sum_schema());
    }

    #[test]
    fn postcard_enum_struct_variant() {
        roundtrip(json!({"Err": {"reason": "kind conflict"}}), &sum_schema());
    }

    #[test]
    fn postcard_enum_unknown_discriminant_errors() {
        // discriminant 99 isn't in the schema.
        let schema = sum_schema();
        let err = decode_schema(&[99], &schema).expect_err("unknown enum discriminant must error");
        assert!(matches!(err, DecodeError::UnknownEnumDiscriminant { .. }));
    }

    #[test]
    fn varint_decodes_at_byte_boundaries() {
        for n in [0u64, 127, 128, 16383, 16384, u64::from(u32::MAX), u64::MAX] {
            let bytes =
                postcard::to_allocvec(&n).expect("test setup: postcard reference varint u64");
            let mut cur = Cursor::new(&bytes);
            let back = read_varint_u64(&mut cur, "$").expect("test setup: read varint u64");
            assert_eq!(back, n, "varint decode mismatch for {n}");
        }
    }

    #[test]
    fn varint_overflow_errors() {
        // 11 continuation bytes — exceeds u64.
        let bytes = vec![0xff; 11];
        let mut cur = Cursor::new(&bytes);
        let err = read_varint_u64(&mut cur, "$").expect_err("varint exceeding 10 bytes must error");
        assert!(matches!(err, DecodeError::VarintOverflow { .. }));
    }

    #[test]
    fn zigzag_decodes_to_signed() {
        for n in [
            0i64,
            -1,
            1,
            -128,
            127,
            i64::from(i32::MIN),
            i64::from(i32::MAX),
        ] {
            let bytes =
                postcard::to_allocvec(&n).expect("test setup: postcard reference zigzag i64");
            let mut cur = Cursor::new(&bytes);
            let raw = read_varint_u64(&mut cur, "$").expect("test setup: read zigzag varint");
            assert_eq!(unzigzag(raw), n, "zigzag mismatch for {n}");
        }
    }

    #[test]
    fn nan_and_infinity_decode_to_null() {
        // Encoder writes raw f64 bytes; decoder coerces non-finite to
        // null so the JSON value is always valid.
        let schema = pc_struct(vec![scalar("x", Primitive::F64)]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&f64::NAN.to_le_bytes());
        let v = decode_schema(&bytes, &schema).expect("test setup: decode NaN f64");
        assert_eq!(v, json!({"x": null}));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&f64::INFINITY.to_le_bytes());
        let v = decode_schema(&bytes, &schema).expect("test setup: decode infinity f64");
        assert_eq!(v, json!({"x": null}));
    }

    #[test]
    fn finite_f64_roundtrips_exactly() {
        let schema = pc_struct(vec![scalar("x", Primitive::F64)]);
        for n in [0.0, 1.5, -123.456, f64::MIN_POSITIVE, f64::MAX] {
            roundtrip(json!({"x": n}), &schema);
        }
    }

    // Issue #232 — `SchemaType::Map` decode tests. Each pins JSON
    // round-trip equivalence: encoder takes a JSON object, decoder
    // produces the same shape (key strings stringified per proto3).

    fn map_schema(key: SchemaType, value: SchemaType) -> SchemaType {
        SchemaType::Map {
            key: SchemaCell::owned(key),
            value: SchemaCell::owned(value),
        }
    }

    #[test]
    fn map_string_keys_roundtrip() {
        roundtrip(
            json!({"content-type": "application/json", "x-trace": "abc123"}),
            &map_schema(SchemaType::String, SchemaType::String),
        );
    }

    #[test]
    fn map_u32_keys_roundtrip() {
        // Decoder emits integer keys as decimal-string JSON keys —
        // matches the encoder's input shape, so round-trip is exact.
        roundtrip(
            json!({"1": "one", "42": "answer", "255": "max"}),
            &map_schema(SchemaType::Scalar(Primitive::U32), SchemaType::String),
        );
    }

    #[test]
    fn map_i64_keys_roundtrip() {
        roundtrip(
            json!({"-1": "neg", "0": "zero", "9223372036854775807": "max"}),
            &map_schema(SchemaType::Scalar(Primitive::I64), SchemaType::String),
        );
    }

    #[test]
    fn map_bool_keys_roundtrip() {
        roundtrip(
            json!({"false": 0u32, "true": 1u32}),
            &map_schema(SchemaType::Bool, SchemaType::Scalar(Primitive::U32)),
        );
    }

    #[test]
    fn map_inside_struct_field_roundtrip() {
        // The expected shape for the named v1 use case: a map field
        // inside a postcard struct (HTTP-header-style descriptor).
        let schema = pc_struct(vec![NamedField {
            name: "headers".into(),
            ty: map_schema(SchemaType::String, SchemaType::String),
        }]);
        roundtrip(
            json!({"headers": {"x-foo": "bar", "x-baz": "qux"}}),
            &schema,
        );
    }

    #[test]
    fn map_empty_roundtrip() {
        roundtrip(
            json!({}),
            &map_schema(SchemaType::String, SchemaType::String),
        );
    }

    #[test]
    fn map_inside_cast_struct_rejected() {
        let schema = cast_struct(vec![NamedField {
            name: "headers".into(),
            ty: map_schema(SchemaType::String, SchemaType::String),
        }]);
        // 1-byte payload is enough to fail at the field-walk step.
        let err = decode_schema(&[0], &schema).expect_err("map inside cast struct must error");
        assert!(matches!(err, DecodeError::UnsupportedSchema(_)));
    }

    // ADR-0065: typed-id round-trips through both wire shapes.

    #[test]
    fn type_id_postcard_round_trips_as_tagged_string() {
        // JSON in: tagged string. Wire: u64 varint. JSON out: same
        // tagged string. The post-migration shape an agent sees end
        // to end.
        let schema = pc_struct(vec![NamedField {
            name: "mailbox".into(),
            ty: SchemaType::TypeId(aether_data::MailboxId::TYPE_ID),
        }]);
        let mailbox = aether_data::MailboxId::from_name("aether.component");
        let s = tagged_id::encode(mailbox.0).expect("test setup: encode tagged mailbox id");
        roundtrip(json!({ "mailbox": s }), &schema);
    }

    #[test]
    fn type_id_cast_round_trips_as_tagged_string() {
        // Same as above but with a `repr_c: true` parent so the
        // cast-shape path runs (8 bytes LE at 8-byte align).
        let schema = cast_struct(vec![
            NamedField {
                name: "stream".into(),
                ty: SchemaType::Scalar(Primitive::U8),
            },
            NamedField {
                name: "mailbox".into(),
                ty: SchemaType::TypeId(aether_data::MailboxId::TYPE_ID),
            },
        ]);
        let mailbox = aether_data::MailboxId::from_name("aether.component");
        let s = tagged_id::encode(mailbox.0).expect("test setup: encode tagged mailbox id");
        roundtrip(json!({ "stream": 1, "mailbox": s }), &schema);
    }

    #[test]
    fn subscribe_input_kind_round_trips_with_tagged_mailbox() {
        // End-to-end through the `SubscribeInput` kind's actual
        // schema — mirrors the worked example in ADR-0065's Context
        // section. ADR-0068: the field is now `kind: KindId` (tagged
        // string on the JSON side), keying subscriber sets by kind id
        // directly. An agent receives the tagged ids from
        // `load_component` / `describe_kinds`, drops them straight
        // into `subscribe_input.{kind, mailbox}`, and the wire bytes
        // match what the substrate expects.
        use aether_data::Kind;
        let mailbox = aether_data::MailboxId::from_name("aether.component");
        let mailbox_str =
            tagged_id::encode(mailbox.0).expect("test setup: encode tagged mailbox id");
        let kind_id = aether_kinds::Tick::ID;
        let kind_str = tagged_id::encode(kind_id.0).expect("test setup: encode tagged kind id");
        let json_in = json!({ "kind": kind_str, "mailbox": mailbox_str });

        let bytes = encode_schema(
            &json_in,
            &<aether_kinds::SubscribeInput as aether_data::Schema>::SCHEMA,
        )
        .expect("encode subscribe_input via TypeId schema");

        // Substrate decode path — wire is byte-identical to a
        // hand-postcard'd `SubscribeInput`.
        let decoded: aether_kinds::SubscribeInput = postcard::from_bytes(&bytes)
            .expect("postcard decode subscribe_input from hub-encoded bytes");
        assert_eq!(decoded.kind, kind_id);
        assert_eq!(decoded.mailbox, mailbox);

        // And the kind's id is sensitive to the typed identity —
        // ADR-0065 phase 3 shifts it from the previous `u64`-shape
        // hash. Cross-check it lands on a `Tag::Kind` value (the
        // `with_tag` discipline holds through the schema-bytes
        // change).
        assert_eq!(
            tagged_id::tag_of(aether_kinds::SubscribeInput::ID.0),
            Some(aether_data::Tag::Kind),
        );
    }

    #[test]
    fn type_id_round_trip_of_sentinel_uses_back_compat_number() {
        // `MailboxId::NONE` (= 0) has reserved tag bits, so it
        // serialises as a JSON number. Round-trip preserves the
        // sentinel value end to end.
        let schema = pc_struct(vec![NamedField {
            name: "mailbox".into(),
            ty: SchemaType::TypeId(aether_data::MailboxId::TYPE_ID),
        }]);
        roundtrip(json!({ "mailbox": 0u64 }), &schema);
    }

    #[test]
    fn ref_inline_cast_inner_wire_is_length_prefixed_cast_image() {
        // ADR-0100: the inline body of a cast inner kind is the raw
        // cast image, length-prefixed — not a postcard varint image.
        let inner = cast_struct(vec![scalar("code", Primitive::U32)]);
        let schema = SchemaType::Ref(SchemaCell::owned(inner.clone()));
        let value = json!({ "Inline": { "code": 0x0102_0304u32 } });

        let bytes = encode_schema(&value, &schema).expect("encode Inline Ref");
        // The inner image is exactly what `encode_schema` emits for the
        // inner kind standalone (the cast image).
        let body = encode_schema(&json!({ "code": 0x0102_0304u32 }), &inner).expect("encode inner");
        assert_eq!(body.len(), 4, "u32 cast image is 4 raw bytes");
        let mut expected = vec![0u8, 4u8];
        expected.extend_from_slice(&body);
        assert_eq!(
            bytes, expected,
            "inline wire is disc 0 + varint(len) + cast image"
        );

        // JSON descriptor round-trips through wire and back.
        let back = decode_schema(&bytes, &schema).expect("decode Inline Ref");
        assert_eq!(back, value);
    }

    // Issue #1586 — bound `decode_schema` collection allocations. A
    // wire-decoded length must not drive the decoder into an unbounded
    // allocation; the four classes below pin the fix.

    /// (a) The `ASan` repro class (#1562 fuzz crash): a varint of
    /// `u32::MAX` followed by an empty tail against `Vec<u32>` and
    /// `Map<u32, u32>`. The pre-allocation clamp keeps `with_capacity`
    /// from requesting an exabyte; the decode then errors `Truncated`
    /// reading the first absent element rather than aborting the process.
    #[test]
    fn oversized_collection_length_errors_without_allocating() {
        let len_bytes =
            postcard::to_allocvec(&u64::from(u32::MAX)).expect("test setup: varint u32::MAX");

        let vec_schema = SchemaType::Vec(SchemaCell::owned(SchemaType::Scalar(Primitive::U32)));
        let err = decode_schema(&len_bytes, &vec_schema)
            .expect_err("oversized Vec length must error, not allocate");
        assert!(matches!(err, DecodeError::Truncated { .. }));

        let map = map_schema(
            SchemaType::Scalar(Primitive::U32),
            SchemaType::Scalar(Primitive::U32),
        );
        let err = decode_schema(&len_bytes, &map)
            .expect_err("oversized Map length must error, not allocate");
        assert!(matches!(err, DecodeError::Truncated { .. }));
    }

    /// (b) The bomb class the 2026-06-10 bounce identified: a huge count
    /// of zero-wire-byte elements (`Unit`, field-less `Struct`). The
    /// clamp can't help — each loop iteration consumes no input yet
    /// allocates a `Value` — so the decode-wide value budget is what
    /// stops it with `ValueBudgetExceeded`.
    #[test]
    fn zero_byte_element_bomb_exceeds_value_budget() {
        let count_bytes =
            postcard::to_allocvec(&u64::from(u32::MAX)).expect("test setup: varint count");

        let unit_vec = SchemaType::Vec(SchemaCell::owned(SchemaType::Unit));
        let err = decode_schema(&count_bytes, &unit_vec)
            .expect_err("Vec<Unit> bomb must exceed the value budget");
        assert!(matches!(err, DecodeError::ValueBudgetExceeded { .. }));

        let struct_vec = SchemaType::Vec(SchemaCell::owned(pc_struct(vec![])));
        let err = decode_schema(&count_bytes, &struct_vec)
            .expect_err("Vec<Struct {}> bomb must exceed the value budget");
        assert!(matches!(err, DecodeError::ValueBudgetExceeded { .. }));
    }

    /// (c) The bounce's valid-input counterexample: a single field-less
    /// struct element, `[{}]`, must still round-trip. The rejected
    /// `len > remaining` guard would have refused this (the element is
    /// zero wire bytes); the clamp + budget approach leaves it valid.
    #[test]
    fn vec_of_one_empty_struct_roundtrips() {
        let schema = SchemaType::Vec(SchemaCell::owned(pc_struct(vec![])));
        roundtrip(json!([{}]), &schema);
    }

    /// (d) A moderate zero-wire-byte-element collection (≈100 `Unit`s)
    /// sits well inside the base budget and round-trips.
    #[test]
    fn vec_of_hundred_units_roundtrips_inside_base_budget() {
        let schema = SchemaType::Vec(SchemaCell::owned(SchemaType::Unit));
        roundtrip(Value::Array(vec![Value::Null; 100]), &schema);
    }

    #[test]
    fn ref_handle_wire_is_byte_unchanged() {
        let inner = cast_struct(vec![scalar("code", Primitive::U32)]);
        let schema = SchemaType::Ref(SchemaCell::owned(inner));
        let value = json!({ "Handle": { "id": 7u64, "kind_id": 42u64 } });

        let bytes = encode_schema(&value, &schema).expect("encode Handle Ref");
        // disc 1 + varint(7) + varint(42) — unchanged from ADR-0045.
        assert_eq!(bytes, vec![1u8, 7u8, 42u8]);

        let back = decode_schema(&bytes, &schema).expect("decode Handle Ref");
        assert_eq!(back, value);
    }
}
