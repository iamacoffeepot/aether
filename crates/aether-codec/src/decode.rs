// Wire-decode: bytes laid out per `SchemaType` → serde_json. The
// narrowing / sign casts (`u8 → i8` in the cast-shape path) are the
// load-bearing inverse of the encode path; `From::from` / `try_into`
// would obscure the byte-layout contract this function implements.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

// `decode_schema`: wire bytes + `SchemaType` descriptor → serde_json
// value the agent can read directly. Mirror of `encoder::encode_schema`
// — same two paths, picked the same way:
//
// 1. Cast-shaped (`Struct { repr_c: true }` and the recursive tree
//    under it): walk `#[repr(C)]` byte layout, lift each scalar / fixed
//    array into JSON. Encoder pads to alignment between fields and
//    rounds total size to the largest field alignment; the decoder does
//    the same skips. No version byte (the cast image is its own codec).
//
// 2. Wire (everything else): consume the ADR-0118 `aether_data::wire`
//    format directly — fixed-width little-endian scalars, `u32`
//    lengths/selectors, presence bytes, length-prefixed
//    strings/vecs/bytes, externally-tagged enums, and encoded-key-sorted
//    maps. The image is unversioned.
//
// We decode the bytes directly rather than going through serde's
// deserializer because the descriptor is structural (not a typed
// schema), and the encoder writes bytes directly for the same reason.
// Round-trip tests against the encoder — and the adapter-vs-walker
// conformance test — pin the wire format from both sides.

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
    /// unbounded allocation. [`decode_schema_strict`] reuses this for
    /// its caller-named ceiling, so `budget` is whichever of the two
    /// this decode ran under.
    ValueBudgetExceeded {
        path: String,
        budget: usize,
    },
    /// A `F32` / `F64` whose bytes are NaN or an infinity, under the
    /// strict policy only ([`decode_schema_strict`]). The compatibility
    /// policy projects the same bytes as `null`.
    NonFiniteFloat {
        path: String,
    },
    /// A decoded `Map` rendered the same JSON key twice, under the
    /// strict policy only ([`decode_schema_strict`]). The compatibility
    /// policy lets the later entry overwrite the earlier one.
    DuplicateMapKey {
        path: String,
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
            Self::TrailingBytes { path, remaining } => {
                write!(f, "trailing bytes after decoding {path}: {remaining} unread")
            }
            Self::InvalidBool { path, byte } => {
                write!(f, "invalid bool at {path}: 0x{byte:02x} not 0 or 1")
            }
            Self::InvalidUtf8 { path } => write!(f, "invalid utf-8 in string at {path}"),
            Self::ValueBudgetExceeded { path, budget } => {
                write!(f, "decode value budget exceeded at {path}: more than {budget} values")
            }
            Self::NonFiniteFloat { path } => write!(f, "non-finite float at {path}: JSON has no NaN or infinity"),
            Self::DuplicateMapKey { path } => write!(f, "duplicate map key at {path}"),
            Self::UnknownEnumDiscriminant { path, discriminant } => {
                write!(f, "enum at {path} has no variant for discriminant {discriminant}")
            }
            Self::UnsupportedSchema(shape) => {
                write!(f, "schema arm not supported by hub decoder: {shape}")
            }
        }
    }
}

impl error::Error for DecodeError {}

/// Decode-side allocation budget, in the spirit of `frame.rs`'s
/// `MAX_FRAME_SIZE`: a wire-decoded length must never drive the decoder
/// into an unbounded allocation. Every structured node charges one value
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

/// Which of the two decode policies a `Cursor` carries. Both walk the
/// identical wire format; they differ only in what they do with the
/// values that format admits but a boundary shouldn't forward.
///
/// The codec's compatibility domain is deliberately wider than any one
/// caller's: `decode_schema` answers "what did the encoder write", so a
/// non-finite float becomes `null`, a repeated map key resolves
/// last-writer-wins, and the value budget only has to be
/// bomb-proof — it is derived from the input length rather than named
/// by the caller. A protocol boundary answers "may this leave the
/// process", which is narrower on all three counts, so
/// `decode_schema_strict` names them as errors instead of choices.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    Compatibility,
    Strict,
}

/// ADR-0020 / ADR-0118: decode `bytes` against a `SchemaType`
/// descriptor into a JSON value symmetric to what `encode_schema`
/// would accept. Dispatches on the schema's wire shape (same split as
/// the encoder):
///
/// - `Unit` → `null` (bare empty payload).
/// - `Struct { repr_c: true }` (and the recursive cast-shaped tree
///   under it) → walk the `#[repr(C)]` byte layout.
/// - Everything else → consume the `aether_data::wire` format directly.
///
/// The encoding is unversioned (ADR-0118 §Envelope).
///
/// Trailing bytes are an error (the encoder writes exactly the right
/// number of bytes; extras mean schema/payload drift the agent should
/// see).
pub fn decode_schema(bytes: &[u8], schema: &SchemaType) -> Result<Value, DecodeError> {
    decode_root(Cursor::new(bytes), schema)
}

/// [`decode_schema`] under the strict policy: the same wire-format walk
/// and the same two shapes, with the three checks a protocol boundary
/// needs before it forwards provider bytes to a client.
///
/// - A non-finite `F32` / `F64` is [`DecodeError::NonFiniteFloat`]
///   rather than `null`. The check happens during the decode because it
///   can't happen after one: an `Option::Some(NaN)` projects to exactly
///   the `null` that `None` projects to, so nothing downstream of the
///   walk can still tell them apart.
/// - `maximum_values` replaces the input-proportional budget as the
///   ceiling on projected `Value` nodes, and covers the nodes the
///   compatibility budget never charges — cast-shaped nodes, and the
///   one integer per byte a `Bytes` leaf projects. Collection
///   pre-allocation is clamped to what is left of it, so a crafted
///   length can't reserve past the ceiling before the charge rejects
///   it.
/// - A `Map` whose rendered keys repeat is
///   [`DecodeError::DuplicateMapKey`] rather than a JSON object with
///   one of the two values silently dropped.
///
/// [`decode_schema`] keeps its behavior exactly; a caller that wants
/// the codec's wider compatibility domain still has it.
pub fn decode_schema_strict(bytes: &[u8], schema: &SchemaType, maximum_values: usize) -> Result<Value, DecodeError> {
    decode_root(Cursor::strict(bytes, maximum_values), schema)
}

fn decode_root(mut cur: Cursor<'_>, schema: &SchemaType) -> Result<Value, DecodeError> {
    let value = decode_value(&mut cur, schema, "$")?;
    if cur.remaining() != 0 {
        return Err(DecodeError::TrailingBytes { path: "$".into(), remaining: cur.remaining() });
    }
    Ok(value)
}

fn decode_value(cur: &mut Cursor<'_>, schema: &SchemaType, path: &str) -> Result<Value, DecodeError> {
    match schema {
        SchemaType::Unit => Ok(Value::Null),
        SchemaType::Struct { fields, repr_c: true } => {
            // The root object of a cast-shaped tree. Every node below it
            // charges from `decode_cast_field`; the wire path charges
            // from `decode_wire_value` instead.
            cur.charge_projected(1, path)?;
            let obj = decode_cast_struct(cur, fields, path)?;
            let max_align = struct_alignment(fields)?;
            cur.skip_pad_to(max_align);
            Ok(Value::Object(obj))
        }
        _ => decode_wire_value(cur, schema, path),
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

fn decode_cast_field(cur: &mut Cursor<'_>, ty: &SchemaType, path: &str) -> Result<Value, DecodeError> {
    // Non-cast variants share the same error message across encode +
    // decode; `cast::non_cast_variant_error` owns the classification
    // (and its own exhaustiveness check forces new SchemaType variants
    // to declare which side they fall on).
    if let Some(msg) = non_cast_variant_error(ty) {
        return Err(DecodeError::UnsupportedSchema(msg));
    }
    // Exactly one `Value` leaves this call, so one charge covers the
    // whole cast-shaped tree: an array node and each of its elements
    // arrive here in their own call.
    cur.charge_projected(1, path)?;
    match ty {
        SchemaType::Scalar(p) => {
            let a = align_of_primitive(*p);
            cur.skip_pad_to(a);
            read_primitive_le(cur, *p, path)
        }
        SchemaType::Array { element, len } => {
            let elem_align = alignment_of_schema(element)?;
            cur.skip_pad_to(elem_align);
            let mut arr = Vec::with_capacity(cur.clamp_prealloc(*len as usize));
            for i in 0..*len {
                let elem_path = format!("{path}[{i}]");
                arr.push(decode_cast_field(cur, element, &elem_path)?);
            }
            Ok(Value::Array(arr))
        }
        SchemaType::Struct { fields, repr_c: true } => {
            let nested_align = alignment_of_schema(ty)?;
            cur.skip_pad_to(nested_align);
            let obj = decode_cast_struct(cur, fields, path)?;
            let inner_max = struct_alignment(fields)?;
            cur.skip_pad_to(inner_max);
            Ok(Value::Object(obj))
        }
        SchemaType::Struct { repr_c: false, .. } => {
            Err(DecodeError::UnsupportedSchema("Struct { repr_c: false } in cast-shaped parent"))
        }
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
    let _expected =
        aether_data::tag_for_type_id(type_id).ok_or(DecodeError::UnsupportedSchema("unknown TypeId in schema"))?;
    Ok(tagged_id::encode(id).map_or_else(|| Value::from(id), Value::String))
}

/// Fixed-width little-endian read of the declared width — shared by the
/// repr-C cast path and the wire path (the inverse of
/// `encode::write_scalar_wire`). No varints, no zigzag.
fn read_primitive_le(cur: &mut Cursor<'_>, p: Primitive, path: &str) -> Result<Value, DecodeError> {
    let policy = cur.policy;
    match p {
        Primitive::U8 => Ok(Value::from(u8::from_le_bytes(cur.take::<1>(path)?))),
        Primitive::U16 => Ok(Value::from(u16::from_le_bytes(cur.take::<2>(path)?))),
        Primitive::U32 => Ok(Value::from(u32::from_le_bytes(cur.take::<4>(path)?))),
        Primitive::U64 => Ok(Value::from(u64::from_le_bytes(cur.take::<8>(path)?))),
        Primitive::I8 => Ok(Value::from(i8::from_le_bytes(cur.take::<1>(path)?))),
        Primitive::I16 => Ok(Value::from(i16::from_le_bytes(cur.take::<2>(path)?))),
        Primitive::I32 => Ok(Value::from(i32::from_le_bytes(cur.take::<4>(path)?))),
        Primitive::I64 => Ok(Value::from(i64::from_le_bytes(cur.take::<8>(path)?))),
        Primitive::F32 => project_float(f64::from(f32::from_le_bytes(cur.take::<4>(path)?)), policy, path),
        Primitive::F64 => project_float(f64::from_le_bytes(cur.take::<8>(path)?), policy, path),
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
        SchemaType::Struct { fields, repr_c: true } => struct_alignment(fields),
        _ => Err(DecodeError::UnsupportedSchema("alignment query on non-cast schema")),
    }
}

// Schema-driven wire decoder: one match arm per `SchemaType`
// variant. Each arm is short but the arm count adds up — extracting
// per-type helpers obscures the schema → wire mapping that's the
// purpose of this fn. Values are bare interior bytes; the leading
// version byte was stripped by `decode_schema`.
#[allow(clippy::too_many_lines)]
fn decode_wire_value(cur: &mut Cursor<'_>, schema: &SchemaType, path: &str) -> Result<Value, DecodeError> {
    // Every wire node charges exactly once — collection elements,
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
                _ => Err(DecodeError::InvalidBool { path: path.into(), byte: b }),
            }
        }
        SchemaType::Scalar(p) => read_primitive_le(cur, *p, path),
        SchemaType::String => {
            let len = cur.read_count(path)? as usize;
            let bytes = cur.take_slice(len, path)?;
            let s = str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8 { path: path.into() })?;
            Ok(Value::String(s.into()))
        }
        SchemaType::Bytes => {
            let len = cur.read_count(path)? as usize;
            let bytes = cur.take_slice(len, path)?;
            // One `Value` per byte, so the leaf is the densest node in
            // the format — charged before the collect, since a ceiling
            // below `len` must reject rather than allocate.
            cur.charge_projected(len, path)?;
            // Mirror encoder input shape: array of byte values.
            let arr = bytes.iter().map(|b| Value::from(*b)).collect();
            Ok(Value::Array(arr))
        }
        SchemaType::Option(inner) => {
            let [tag] = cur.take::<1>(path)?;
            match tag {
                0 => Ok(Value::Null),
                1 => decode_wire_value(cur, inner, path),
                _ => Err(DecodeError::InvalidBool { path: path.into(), byte: tag }),
            }
        }
        SchemaType::Vec(inner) => {
            let len = cur.read_count(path)? as usize;
            // Clamp the pre-allocation against the bytes that remain: a
            // wire-encoded element occupies ≥ 1 byte, so a `len` past
            // `remaining` can't be valid non-degenerate input. Zero-byte
            // elements start small and grow by push; the decode-wide
            // budget bounds that loop.
            let mut arr = Vec::with_capacity(cur.clamp_prealloc(len.min(cur.remaining())));
            for i in 0..len {
                let elem_path = format!("{path}[{i}]");
                arr.push(decode_wire_value(cur, inner, &elem_path)?);
            }
            Ok(Value::Array(arr))
        }
        SchemaType::Array { element, len } => {
            let mut arr = Vec::with_capacity(cur.clamp_prealloc(*len as usize));
            for i in 0..*len {
                let elem_path = format!("{path}[{i}]");
                arr.push(decode_wire_value(cur, element, &elem_path)?);
            }
            Ok(Value::Array(arr))
        }
        SchemaType::Struct { fields, .. } => {
            // Wire struct: concatenated field bytes in declaration order.
            let mut obj = Map::with_capacity(fields.len());
            for field in fields.iter() {
                let field_path = format!("{path}.{}", field.name);
                let value = decode_wire_value(cur, &field.ty, &field_path)?;
                obj.insert(field.name.to_string(), value);
            }
            Ok(Value::Object(obj))
        }
        SchemaType::Enum { variants } => {
            let disc = cur.read_count(path)?;
            let variant = variants
                .iter()
                .find(|v| v.discriminant() == disc)
                .ok_or_else(|| DecodeError::UnknownEnumDiscriminant { path: path.into(), discriminant: disc })?;
            decode_enum_body(cur, variant, path)
        }
        SchemaType::Map { key: key_schema, value: value_schema } => {
            // Issue #232 + proto3-style JSON mapping. Wire is the
            // `aether_data::wire` `Map` shape — `u32(count)` followed by
            // `(k, v)` pairs in ascending encoded-key byte order. We emit
            // a JSON object with the proto3 stringify rule: integer keys
            // as decimal-string keys, bool keys as `"true"`/`"false"`,
            // string keys identity. Order in the emitted object isn't
            // load-bearing for decoders that compare by value.
            let len = cur.read_count(path)? as usize;
            // Same clamp as the `Vec` arm: a `(k, v)` pair occupies ≥ 1
            // byte, so cap the pre-allocation at the bytes remaining.
            let mut obj = Map::with_capacity(cur.clamp_prealloc(len.min(cur.remaining())));
            for i in 0..len {
                let entry_path = format!("{path}[{i}]");
                let key_value = decode_wire_value(cur, key_schema, &entry_path)?;
                let val_value = decode_wire_value(cur, value_schema, &entry_path)?;
                let key_string = render_map_key(&key_value, key_schema, &entry_path)?;
                // Distinct encoded keys can still render to one JSON
                // key, and the insert would drop a value either way. The
                // compatibility policy keeps the later one; the strict
                // policy won't forward a map the caller can't read back.
                if cur.policy == Policy::Strict && obj.contains_key(&key_string) {
                    return Err(DecodeError::DuplicateMapKey { path: entry_path });
                }
                obj.insert(key_string, val_value);
            }
            Ok(Value::Object(obj))
        }
        SchemaType::TypeId(type_id) => {
            // ADR-0065 typed id. Wire is a `u64` fixed little-endian;
            // emit the tagged string form (or back-compat number for
            // reserved-tag sentinels).
            let id = u64::from_le_bytes(cur.take::<8>(path)?);
            render_type_id_value(id, *type_id, path)
        }
    }
}

fn decode_enum_body(cur: &mut Cursor<'_>, variant: &EnumVariant, path: &str) -> Result<Value, DecodeError> {
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
                decode_wire_value(cur, &fields[0], &nested_path)?
            } else {
                let mut arr = Vec::with_capacity(fields.len());
                for (i, ty) in fields.iter().enumerate() {
                    let nested = format!("{path}::{name}.{i}");
                    arr.push(decode_wire_value(cur, ty, &nested)?);
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
                let v = decode_wire_value(cur, &field.ty, &nested)?;
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
/// scalars to decimal digits, bool to `"true"`/`"false"`, a fieldless
/// enum to its variant name. Anything else
/// is `UnsupportedSchema` — the `BTreeMap`<K: Ord, V> bound at the Rust
/// layer makes those unreachable, but the codec rejects them defensively
/// in case a descriptor lands here from an external source.
fn render_map_key(key_value: &Value, key_schema: &SchemaType, path: &str) -> Result<String, DecodeError> {
    match (key_schema, key_value) {
        (SchemaType::String, Value::String(s)) => Ok(s.clone()),
        (SchemaType::Bool, Value::Bool(b)) => Ok(if *b {
            "true".into()
        } else {
            "false".into()
        }),
        (SchemaType::Scalar(p), Value::Number(n)) => match p {
            Primitive::U8 | Primitive::U16 | Primitive::U32 | Primitive::U64 => Ok(n
                .as_u64()
                .ok_or(DecodeError::UnsupportedSchema("decoded unsigned key value out of u64 range"))?
                .to_string()),
            Primitive::I8 | Primitive::I16 | Primitive::I32 | Primitive::I64 => Ok(n
                .as_i64()
                .ok_or(DecodeError::UnsupportedSchema("decoded signed key value out of i64 range"))?
                .to_string()),
            Primitive::F32 | Primitive::F64 => Err(DecodeError::UnsupportedSchema("float as Map key (no Ord)")),
        },
        // The decode already resolved the discriminant to a variant, so a key
        // that arrives here as a string names a known fieldless one — the body
        // check is what rejects a variant whose decode would have produced an
        // object instead.
        (SchemaType::Enum { .. }, Value::String(name)) => Ok(name.clone()),
        _ => {
            let _ = path;
            Err(DecodeError::UnsupportedSchema("Map key must be String, integer scalar, Bool, or fieldless enum"))
        }
    }
}

/// JSON numbers can't represent NaN/infinity. The encoder accepts
/// arbitrary `f64`s, so the decode has to decide what a non-finite one
/// projects to, and the two policies decide differently: the
/// compatibility policy coerces to `null` so the JSON value remains
/// valid (finite floats round trip exactly; NaN/inf bytes decode to
/// null — loud, not silent), while the strict policy refuses to hand a
/// boundary a value that its advertised schema says is a number.
fn project_float(n: f64, policy: Policy, path: &str) -> Result<Value, DecodeError> {
    match (serde_json::Number::from_f64(n), policy) {
        (Some(number), _) => Ok(Value::Number(number)),
        (None, Policy::Compatibility) => Ok(Value::Null),
        (None, Policy::Strict) => Err(DecodeError::NonFiniteFloat { path: path.into() }),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
    policy: Policy,
    /// The budget this decode started with, kept for the error message.
    budget: usize,
    /// Remaining value budget for this decode (see `VALUE_BUDGET_BASE`).
    /// Each wire node decrements it via `charge_value`.
    values_left: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        // Saturating: a `bytes.len()` near `usize::MAX` is not reachable
        // (it's a real slice), but the arithmetic stays defined.
        let budget = VALUE_BUDGET_BASE.saturating_add(bytes.len().saturating_mul(VALUES_PER_INPUT_BYTE));
        Self { bytes, pos: 0, policy: Policy::Compatibility, budget, values_left: budget }
    }

    fn strict(bytes: &'a [u8], maximum_values: usize) -> Self {
        Self { bytes, pos: 0, policy: Policy::Strict, budget: maximum_values, values_left: maximum_values }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Charge one value against the decode-wide budget. Returns
    /// `ValueBudgetExceeded` once the budget is exhausted, so a decode
    /// can't expand into more `Value` nodes than the input length
    /// justifies — the bound for zero-wire-byte-element collections.
    fn charge_value(&mut self, path: &str) -> Result<(), DecodeError> {
        self.charge(1, path)
    }

    /// Charge `count` values that only the strict policy counts: the
    /// cast-shaped nodes and the per-byte `Bytes` values that sit
    /// outside `charge_value`'s per-wire-node accounting. A no-op under
    /// the compatibility policy, whose budget is derived from the input
    /// length and whose behavior must not move.
    fn charge_projected(&mut self, count: usize, path: &str) -> Result<(), DecodeError> {
        match self.policy {
            Policy::Compatibility => Ok(()),
            Policy::Strict => self.charge(count, path),
        }
    }

    fn charge(&mut self, count: usize, path: &str) -> Result<(), DecodeError> {
        match self.values_left.checked_sub(count) {
            Some(remaining) => {
                self.values_left = remaining;
                Ok(())
            }
            None => Err(DecodeError::ValueBudgetExceeded { path: path.into(), budget: self.budget }),
        }
    }

    /// Clamp a collection's pre-allocation to what is left of an
    /// explicit ceiling. The compatibility path keeps its own clamps
    /// (`len.min(remaining)` where a length is wire-read) untouched;
    /// under the strict policy a caller who named a small ceiling must
    /// not see a large reservation before the charge that rejects it.
    fn clamp_prealloc(&self, len: usize) -> usize {
        match self.policy {
            Policy::Compatibility => len,
            Policy::Strict => len.min(self.values_left),
        }
    }

    fn take<const N: usize>(&mut self, path: &str) -> Result<[u8; N], DecodeError> {
        if self.remaining() < N {
            return Err(DecodeError::Truncated { path: path.into(), needed: N, had: self.remaining() });
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    fn take_slice(&mut self, n: usize, path: &str) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated { path: path.into(), needed: n, had: self.remaining() });
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Read a `u32` little-endian count/length/selector — the `wire`
    /// framing for string / bytes / vec / map lengths and the enum
    /// discriminant.
    fn read_count(&mut self, path: &str) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take::<4>(path)?))
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
    use crate::test_fixtures::{cast_struct, named, pending_ok_err_variants, scalar, structured_struct};
    use aether_data::SchemaCell;
    use aether_data::tagged_id;
    use serde_json::json;

    /// Local alias preserving the decode-side spelling that the test
    /// bodies below already use.
    fn pc_struct(fields: Vec<NamedField>) -> SchemaType {
        structured_struct(fields)
    }

    /// Encode → decode → assert equal. The single most load-bearing
    /// invariant: every kind shape the encoder accepts, the decoder
    /// inverts.
    // `value` is owned because the test passes a freshly-built `Value`
    // (e.g. `Value::String("…".to_owned())`) inline at the call site;
    // taking `&Value` would force ad-hoc bindings at every site.
    #[allow(clippy::needless_pass_by_value)]
    fn roundtrip(value: Value, schema: &SchemaType) {
        let bytes = encode_schema(&value, schema).unwrap_or_else(|e| panic!("encode failed for {value:?}: {e}"));
        let back = decode_schema(&bytes, schema).unwrap_or_else(|e| panic!("decode failed for {value:?}: {e}"));
        assert_eq!(back, value, "round-trip mismatch for {value:?}");
    }

    #[test]
    fn unit_decodes_null() {
        let v = decode_schema(&[], &SchemaType::Unit).expect("test setup: decode empty unit");
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn unit_rejects_trailing_bytes() {
        let err = decode_schema(&[1, 2, 3], &SchemaType::Unit).expect_err("trailing bytes after unit must error");
        assert!(matches!(err, DecodeError::TrailingBytes { .. }));
    }

    // Cast-shaped path

    #[test]
    fn cast_single_u32() {
        roundtrip(json!({"code": 42u32}), &cast_struct(vec![scalar("code", Primitive::U32)]));
    }

    #[test]
    fn cast_two_f32_fields() {
        roundtrip(
            json!({"x": 1.5, "y": -3.25}),
            &cast_struct(vec![scalar("x", Primitive::F32), scalar("y", Primitive::F32)]),
        );
    }

    #[test]
    fn cast_padding_between_u8_and_u32() {
        roundtrip(
            json!({"a": 7u8, "b": 0x0102_0304u32}),
            &cast_struct(vec![scalar("a", Primitive::U8), scalar("b", Primitive::U32)]),
        );
    }

    #[test]
    fn cast_trailing_padding_for_u64_then_u8() {
        // Encoder pads to 16 bytes; decoder must skip the trailing 7
        // zeros before checking for trailing bytes.
        roundtrip(
            json!({"a": 1u64, "b": 2u8}),
            &cast_struct(vec![scalar("a", Primitive::U64), scalar("b", Primitive::U8)]),
        );
    }

    #[test]
    fn cast_fixed_array_field() {
        roundtrip(
            json!({"xs": [1u8, 2, 3, 4]}),
            &cast_struct(vec![NamedField {
                name: "xs".into(),
                ty: SchemaType::Array { element: SchemaCell::owned(SchemaType::Scalar(Primitive::U8)), len: 4 },
            }]),
        );
    }

    #[test]
    fn cast_signed_negative_roundtrip() {
        roundtrip(json!({"n": -1}), &cast_struct(vec![scalar("n", Primitive::I32)]));
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
            ty: SchemaType::Array { element: SchemaCell::owned(vertex), len: 3 },
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

    // Structured path — primitives

    #[test]
    fn structured_bool_field() {
        roundtrip(json!({"flag": true}), &pc_struct(vec![NamedField { name: "flag".into(), ty: SchemaType::Bool }]));
        roundtrip(json!({"flag": false}), &pc_struct(vec![NamedField { name: "flag".into(), ty: SchemaType::Bool }]));
    }

    #[test]
    fn structured_invalid_bool_byte_errors() {
        let schema = pc_struct(vec![NamedField { name: "flag".into(), ty: SchemaType::Bool }]);
        let err = decode_schema(&[2], &schema).expect_err("non-0/1 bool byte must error");
        assert!(matches!(err, DecodeError::InvalidBool { .. }));
    }

    #[test]
    fn structured_string_field() {
        roundtrip(
            json!({"body": "hello world"}),
            &pc_struct(vec![NamedField { name: "body".into(), ty: SchemaType::String }]),
        );
    }

    #[test]
    fn structured_string_invalid_utf8_errors() {
        let schema = pc_struct(vec![NamedField { name: "body".into(), ty: SchemaType::String }]);
        // u32 length 2, then two invalid utf-8 bytes.
        let err = decode_schema(&[2, 0, 0, 0, 0xff, 0xfe], &schema).expect_err("invalid utf-8 string body must error");
        assert!(matches!(err, DecodeError::InvalidUtf8 { .. }));
    }

    #[test]
    fn structured_bytes_field() {
        roundtrip(
            json!({"blob": [1u8, 2, 3, 4, 5]}),
            &pc_struct(vec![NamedField { name: "blob".into(), ty: SchemaType::Bytes }]),
        );
    }

    #[test]
    fn structured_option_some_and_none() {
        let schema = pc_struct(vec![NamedField {
            name: "name".into(),
            ty: SchemaType::Option(SchemaCell::owned(SchemaType::String)),
        }]);
        roundtrip(json!({"name": "Aether"}), &schema);
        roundtrip(json!({"name": null}), &schema);
    }

    #[test]
    fn structured_vec_of_strings() {
        let schema = pc_struct(vec![NamedField {
            name: "tags".into(),
            ty: SchemaType::Vec(SchemaCell::owned(SchemaType::String)),
        }]);
        roundtrip(json!({"tags": ["alpha", "beta", "gamma"]}), &schema);
    }

    #[test]
    fn structured_vec_of_nested_structs() {
        let inner = pc_struct(vec![scalar("seq", Primitive::U32)]);
        let schema =
            pc_struct(vec![NamedField { name: "items".into(), ty: SchemaType::Vec(SchemaCell::owned(inner)) }]);
        roundtrip(json!({"items": [{"seq": 1u32}, {"seq": 256u32}, {"seq": 0xDEADu32}]}), &schema);
    }

    fn sum_schema() -> SchemaType {
        SchemaType::Enum { variants: pending_ok_err_variants().into() }
    }

    #[test]
    fn structured_enum_unit_variant_decodes_as_string_tag() {
        roundtrip(json!("Pending"), &sum_schema());
    }

    #[test]
    fn structured_enum_tuple_single_field_decodes_unwrapped() {
        // Encoder accepts both `{"Ok": 42}` and `{"Ok": [42]}` for
        // single-field tuples; decoder normalizes to the unwrapped
        // form so round-trip from `{"Ok": 42}` is byte-equal.
        roundtrip(json!({"Ok": 42u64}), &sum_schema());
    }

    #[test]
    fn structured_enum_struct_variant() {
        roundtrip(json!({"Err": {"reason": "kind conflict"}}), &sum_schema());
    }

    #[test]
    fn structured_enum_unknown_discriminant_errors() {
        // discriminant 99 isn't in the schema; the u32 selector is
        // little-endian.
        let schema = sum_schema();
        let err = decode_schema(&[99, 0, 0, 0], &schema).expect_err("unknown enum discriminant must error");
        assert!(matches!(err, DecodeError::UnknownEnumDiscriminant { .. }));
    }

    #[test]
    fn scalars_are_fixed_width_little_endian() {
        // u16/u32/i16/i32 are the declared width LE, not varints — pin a
        // couple of round-trips that a varint layout would have shrunk.
        roundtrip(
            json!({"a": 256u16, "b": -2i32}),
            &pc_struct(vec![scalar("a", Primitive::U16), scalar("b", Primitive::I32)]),
        );
    }

    #[test]
    fn nan_and_infinity_decode_to_null() {
        // Encoder writes raw f64 bytes; decoder coerces non-finite to
        // null so the JSON value is always valid.
        let schema = pc_struct(vec![scalar("x", Primitive::F64)]);
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&f64::NAN.to_le_bytes());
        let v = decode_schema(&bytes, &schema).expect("test setup: decode NaN f64");
        assert_eq!(v, json!({"x": null}));

        let mut bytes: Vec<u8> = Vec::new();
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
        SchemaType::Map { key: SchemaCell::owned(key), value: SchemaCell::owned(value) }
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
        // inside a structured struct (HTTP-header-style descriptor).
        let schema = pc_struct(vec![NamedField {
            name: "headers".into(),
            ty: map_schema(SchemaType::String, SchemaType::String),
        }]);
        roundtrip(json!({"headers": {"x-foo": "bar", "x-baz": "qux"}}), &schema);
    }

    #[test]
    fn map_empty_roundtrip() {
        roundtrip(json!({}), &map_schema(SchemaType::String, SchemaType::String));
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
    fn type_id_structured_round_trips_as_tagged_string() {
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
            NamedField { name: "stream".into(), ty: SchemaType::Scalar(Primitive::U8) },
            NamedField { name: "mailbox".into(), ty: SchemaType::TypeId(aether_data::MailboxId::TYPE_ID) },
        ]);
        let mailbox = aether_data::MailboxId::from_name("aether.component");
        let s = tagged_id::encode(mailbox.0).expect("test setup: encode tagged mailbox id");
        roundtrip(json!({ "stream": 1, "mailbox": s }), &schema);
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

    // Issue #1586 — bound `decode_schema` collection allocations. A
    // wire-decoded length must not drive the decoder into an unbounded
    // allocation; the four classes below pin the fix.

    /// (a) The `ASan` repro class (#1562 fuzz crash): a `u32` length of
    /// `u32::MAX` followed by an empty tail against `Vec<u32>` and
    /// `Map<u32, u32>`. The pre-allocation clamp keeps `with_capacity`
    /// from requesting an exabyte; the decode then errors `Truncated`
    /// reading the first absent element rather than aborting the process.
    #[test]
    fn oversized_collection_length_errors_without_allocating() {
        // A `u32` count of `u32::MAX`, no elements.
        let mut len_bytes: Vec<u8> = Vec::new();
        len_bytes.extend_from_slice(&u32::MAX.to_le_bytes());

        let vec_schema = SchemaType::Vec(SchemaCell::owned(SchemaType::Scalar(Primitive::U32)));
        let err = decode_schema(&len_bytes, &vec_schema).expect_err("oversized Vec length must error, not allocate");
        assert!(matches!(err, DecodeError::Truncated { .. }));

        let map = map_schema(SchemaType::Scalar(Primitive::U32), SchemaType::Scalar(Primitive::U32));
        let err = decode_schema(&len_bytes, &map).expect_err("oversized Map length must error, not allocate");
        assert!(matches!(err, DecodeError::Truncated { .. }));
    }

    /// (b) The bomb class the 2026-06-10 bounce identified: a huge count
    /// of zero-wire-byte elements (`Unit`, field-less `Struct`). The
    /// clamp can't help — each loop iteration consumes no input yet
    /// allocates a `Value` — so the decode-wide value budget is what
    /// stops it with `ValueBudgetExceeded`.
    #[test]
    fn zero_byte_element_bomb_exceeds_value_budget() {
        // A `u32` count of `u32::MAX`.
        let mut count_bytes: Vec<u8> = Vec::new();
        count_bytes.extend_from_slice(&u32::MAX.to_le_bytes());

        let unit_vec = SchemaType::Vec(SchemaCell::owned(SchemaType::Unit));
        let err = decode_schema(&count_bytes, &unit_vec).expect_err("Vec<Unit> bomb must exceed the value budget");
        assert!(matches!(err, DecodeError::ValueBudgetExceeded { .. }));

        let struct_vec = SchemaType::Vec(SchemaCell::owned(pc_struct(vec![])));
        let err =
            decode_schema(&count_bytes, &struct_vec).expect_err("Vec<Struct {}> bomb must exceed the value budget");
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

    // `decode_schema_strict` — the boundary policy. The two entry points
    // share one walk, so each test below pins both halves of a policy
    // split: what strict now rejects, and what the compatibility entry
    // point still does with the same bytes.

    #[test]
    fn strict_projects_finite_floats_like_the_compatibility_entry() {
        // Sharing the decoder is what makes the strict entry point cheap
        // and what makes a leak into valid projection possible: an
        // over-eager charge or a mis-ordered finite check would land
        // here, on both wire shapes at once.
        let value = json!({"x": 1.5, "y": -2.25});
        for schema in [
            pc_struct(vec![scalar("x", Primitive::F32), scalar("y", Primitive::F64)]),
            cast_struct(vec![scalar("x", Primitive::F32), scalar("y", Primitive::F64)]),
        ] {
            let bytes = encode_schema(&value, &schema).expect("test setup: encode finite floats");

            assert_eq!(decode_schema(&bytes, &schema).expect("compatibility decode of finite floats"), value);
            assert_eq!(decode_schema_strict(&bytes, &schema, 64).expect("strict decode of finite floats"), value);
        }
    }

    #[test]
    fn strict_rejects_the_non_finite_floats_the_compatibility_entry_nulls() {
        let root = SchemaType::Scalar(Primitive::F64);
        let err = decode_schema_strict(&f64::NAN.to_le_bytes(), &root, 64)
            .expect_err("a non-finite root f64 must not reach the boundary");
        assert!(matches!(err, DecodeError::NonFiniteFloat { ref path } if path == "$"), "{err}");

        // `Some(inf)` is the case a validation pass over the decoded
        // value cannot catch: it projects to exactly the `null` that
        // `None` projects to, so the check has to be inside the walk.
        let optional = SchemaType::Option(SchemaCell::owned(SchemaType::Scalar(Primitive::F32)));
        let mut some_infinity = vec![1u8];
        some_infinity.extend_from_slice(&f32::INFINITY.to_le_bytes());

        assert_eq!(decode_schema(&some_infinity, &optional).expect("compatibility decode of Some(inf)"), Value::Null);
        let err = decode_schema_strict(&some_infinity, &optional, 64)
            .expect_err("a non-finite Some must not reach the boundary");
        assert!(matches!(err, DecodeError::NonFiniteFloat { .. }), "{err}");
    }

    #[test]
    fn strict_value_ceiling_binds_where_the_input_proportional_budget_does_not() {
        // Low enough that every shape below crosses it, while the
        // compatibility budget (4096 + 4/byte) stays far above them —
        // so only a genuinely threaded ceiling can reject these.
        const CEILING: usize = 8;

        // Structured: 64 zero-wire-byte elements plus appended bytes.
        // The compatibility decode walks all 64 and only complains about
        // the tail, which is what the ceiling has to beat.
        let mut unit_vec_bytes = 64u32.to_le_bytes().to_vec();
        unit_vec_bytes.extend_from_slice(&[0u8; 32]);
        let unit_vec = SchemaType::Vec(SchemaCell::owned(SchemaType::Unit));

        let err = decode_schema(&unit_vec_bytes, &unit_vec).expect_err("appended bytes are trailing bytes");
        assert!(matches!(err, DecodeError::TrailingBytes { .. }), "{err}");
        let err = decode_schema_strict(&unit_vec_bytes, &unit_vec, CEILING)
            .expect_err("64 unit values must cross a ceiling of 8");
        assert!(matches!(err, DecodeError::ValueBudgetExceeded { budget: CEILING, .. }), "{err}");

        // Cast-shaped: the compatibility path charges no cast node at
        // all, so these 32 elements are only ever counted by the strict
        // policy.
        let cast_array = cast_struct(vec![named(
            "xs",
            SchemaType::Array { element: SchemaCell::owned(SchemaType::Scalar(Primitive::U8)), len: 32 },
        )]);
        let array_bytes = [7u8; 32];

        assert_eq!(
            decode_schema(&array_bytes, &cast_array).expect("compatibility decode of a cast array"),
            json!({"xs": vec![7u8; 32]})
        );
        let err = decode_schema_strict(&array_bytes, &cast_array, CEILING)
            .expect_err("32 cast array elements must cross a ceiling of 8");
        assert!(matches!(err, DecodeError::ValueBudgetExceeded { .. }), "{err}");

        // `Bytes`: one integer per byte, the densest node in the format.
        let bytes_schema = pc_struct(vec![named("blob", SchemaType::Bytes)]);
        let mut blob_bytes = 32u32.to_le_bytes().to_vec();
        blob_bytes.extend_from_slice(&[1u8; 32]);

        decode_schema(&blob_bytes, &bytes_schema).expect("compatibility decode of a 32-byte blob");
        let err = decode_schema_strict(&blob_bytes, &bytes_schema, CEILING)
            .expect_err("32 byte values must cross a ceiling of 8");
        assert!(matches!(err, DecodeError::ValueBudgetExceeded { .. }), "{err}");
    }

    #[test]
    fn strict_rejects_the_repeated_map_key_the_compatibility_entry_overwrites() {
        // Hand-authored: the encoder can't produce this, because a JSON
        // object can't carry the key twice in the first place. A
        // provider writing wire bytes directly can.
        let schema = map_schema(SchemaType::String, SchemaType::Scalar(Primitive::U32));
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for entry in [1u32, 2u32] {
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.push(b'k');
            bytes.extend_from_slice(&entry.to_le_bytes());
        }

        // Compatibility: last writer wins and the first value is gone.
        assert_eq!(decode_schema(&bytes, &schema).expect("compatibility decode of a repeated key"), json!({"k": 2u32}));

        let err =
            decode_schema_strict(&bytes, &schema, 64).expect_err("a repeated rendered key must not reach the boundary");
        assert!(matches!(err, DecodeError::DuplicateMapKey { .. }), "{err}");
    }
}
