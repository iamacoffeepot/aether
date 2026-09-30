//! Kind → schema resolution and rendering a stored value as JSON.
//!
//! A stored kind resolves in four steps:
//!
//! 1. `bloomery.artifact.text` renders its raw payload as a JSON string, and
//!    `bloomery.artifact.bytes` as `{length, hex}`: both are raw payloads with
//!    no storage encoding.
//! 2. The native storage-kind inventory linked into this binary.
//! 3. The program declarations the driver answers, asked at most once per
//!    request and only when step 2 misses.
//! 4. Otherwise `{kind_id, length, hex}`.
//!
//! A value decodes with [`decode_storage_schema`]. A walk over its schema and
//! the decoded JSON together then rewrites every 32-byte array as a lowercase
//! hex digest and reports where each one sits, so a caller can resolve it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::str;

use aether_bloomery_kinds::{DeclarationsResult, Digest, OpaqueBytes, Utf8Text};
use aether_codec::{DecodeError, decode_storage_schema};
use aether_data::storage::storage_kind;
use aether_data::{EnumVariant, Kind, KindId, Primitive, SchemaType, wire};
use serde_json::{Map, Value};

use super::kinds::MAX_HEX_BYTES;

/// How one stored kind renders.
pub enum Resolved<'a> {
    /// UTF-8 text, rendered as a JSON string.
    Text,
    /// Opaque bytes, rendered as `{length, hex}`.
    Bytes,
    /// A storage-encoded value named `name`, decoded by `schema`.
    Schema { name: &'a str, schema: &'a SchemaType },
    /// No schema source knows the kind.
    Unknown,
}

impl Resolved<'_> {
    /// The kind's name, when a schema source knows it.
    pub fn name(&self) -> Option<String> {
        match self {
            Self::Text => Some(Utf8Text::NAME.to_owned()),
            Self::Bytes => Some(OpaqueBytes::NAME.to_owned()),
            Self::Schema { name, .. } => Some((*name).to_owned()),
            Self::Unknown => None,
        }
    }
}

/// One request's resolver: the fixed sources, and the driver's declarations
/// once they have been asked for.
#[derive(Default)]
pub struct Resolver {
    /// Every declared program kind by id, once the driver has answered.
    declared: Option<HashMap<KindId, (String, SchemaType)>>,
}

impl Resolver {
    /// How `kind` renders, or `None` when only the driver's declarations can
    /// say and they have not been asked for yet.
    pub fn resolve(&self, kind: KindId) -> Option<Resolved<'_>> {
        if kind == Utf8Text::ID {
            return Some(Resolved::Text);
        }
        if kind == OpaqueBytes::ID {
            return Some(Resolved::Bytes);
        }
        if let Some(entry) = storage_kind(kind) {
            return Some(Resolved::Schema { name: entry.name, schema: entry.schema });
        }
        let declared = self.declared.as_ref()?;
        Some(declared.get(&kind).map_or(Resolved::Unknown, |(name, schema)| Resolved::Schema { name, schema }))
    }

    /// Keep the driver's answer: every program's input and result kind whose
    /// schema bytes decode. A schema that does not decode leaves its kind
    /// unknown rather than failing the request.
    pub fn declare(&mut self, result: DeclarationsResult) {
        let mut declared = HashMap::new();
        for program in result.bundles.into_iter().flat_map(|bundle| bundle.programs) {
            for (id, name, schema) in [
                (program.input, program.input_name, program.input_schema),
                (program.result, program.result_name, program.result_schema),
            ] {
                if let Ok(schema) = wire::from_bytes::<SchemaType>(&schema) {
                    declared.entry(id).or_insert((name, schema));
                }
            }
        }
        self.declared = Some(declared);
    }
}

/// One stored value rendered as JSON.
pub struct Rendered {
    /// The value, every digest already hex.
    pub json: Value,
    /// Each digest in the value, as a JSON pointer into `json` and the digest
    /// it holds, in document order.
    pub digests: Vec<(String, Digest)>,
    /// How many JSON values the decode projected, charged to the reply.
    pub values: usize,
    /// Whether a bound cut the rendering short: a hex rendering past
    /// [`MAX_HEX_BYTES`], or a decode past its value budget.
    pub truncated: bool,
    /// Whether the decode ran past its value budget, so `json` is the hex
    /// rendering in its place.
    pub over_budget: bool,
}

/// Render `payload`, stored under `kind` and resolved as `resolved`, decoding
/// at most `values_left` JSON values.
pub fn render(kind: KindId, payload: &[u8], resolved: &Resolved<'_>, values_left: usize) -> Rendered {
    match resolved {
        Resolved::Text => str::from_utf8(payload).map_or_else(
            |_| raw(Some(kind), payload),
            |text| Rendered {
                json: Value::String(text.to_owned()),
                digests: Vec::new(),
                values: 1,
                truncated: false,
                over_budget: false,
            },
        ),
        Resolved::Bytes => raw(None, payload),
        Resolved::Unknown => raw(Some(kind), payload),
        Resolved::Schema { schema, .. } => match decode_storage_schema(payload, schema, values_left) {
            Ok(mut json) => {
                let values = count_values(&json);
                let digests = hex_digests(schema, &mut json);
                Rendered { json, digests, values, truncated: false, over_budget: false }
            }
            Err(DecodeError::ValueBudgetExceeded { .. }) => {
                Rendered { truncated: true, over_budget: true, ..raw(Some(kind), payload) }
            }
            Err(error) => {
                let mut rendered = raw(Some(kind), payload);
                if let Value::Object(object) = &mut rendered.json {
                    object.insert("error".to_owned(), Value::String(error.to_string()));
                }
                rendered
            }
        },
    }
}

/// `payload` as `{length, hex}`, led by `kind_id` for a kind no schema
/// source decodes, with at most [`MAX_HEX_BYTES`] bytes of hex.
fn raw(kind: Option<KindId>, payload: &[u8]) -> Rendered {
    let mut object = Map::new();
    if let Some(kind) = kind {
        object.insert("kind_id".to_owned(), Value::from(kind.0));
    }
    object.insert("length".to_owned(), Value::from(payload.len()));
    object.insert("hex".to_owned(), Value::String(hex(&payload[..payload.len().min(MAX_HEX_BYTES)])));
    Rendered {
        json: Value::Object(object),
        digests: Vec::new(),
        values: 1,
        truncated: payload.len() > MAX_HEX_BYTES,
        over_budget: false,
    }
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// How many JSON values `value` holds, itself included, counted without
/// recursion.
pub fn count_values(value: &Value) -> usize {
    let mut count = 0;
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        count += 1;
        match value {
            Value::Array(items) => pending.extend(items),
            Value::Object(object) => pending.extend(object.values()),
            _ => {}
        }
    }
    count
}

/// Rewrite every `[u8; 32]` position `schema` gives `json` as a lowercase hex
/// string, and return each position's JSON pointer and digest in document
/// order.
///
/// The walk follows the storage decoder's JSON shapes: a struct is an object
/// of its fields, a unit variant is its name, any other variant is
/// `{name: body}`, an `Option` is `null` or its inner value, and a map is an
/// object keyed by rendered keys. It is iterative, with the schema's own depth
/// already capped by the decoder.
fn hex_digests(schema: &SchemaType, json: &mut Value) -> Vec<(String, Digest)> {
    let mut found = Vec::new();
    let mut pending: Vec<(&SchemaType, &Value, String)> = vec![(schema, &*json, String::new())];
    while let Some((schema, value, pointer)) = pending.pop() {
        if let Some(digest) = digest_at(schema, value) {
            found.push((pointer, digest));
            continue;
        }
        // Children are pushed in reverse so they pop in document order.
        let mut children: Vec<(&SchemaType, &Value, String)> = Vec::new();
        match (schema, value) {
            (SchemaType::Struct { fields, .. }, Value::Object(object)) => {
                for field in fields.iter() {
                    if let Some(child) = object.get(field.name.as_ref()) {
                        children.push((&field.ty, child, child_pointer(&pointer, &field.name)));
                    }
                }
            }
            (SchemaType::Enum { variants }, Value::Object(object)) => {
                let Some((name, body)) = object.iter().next() else {
                    continue;
                };
                let Some(variant) = variants.iter().find(|variant| variant.name() == name) else {
                    continue;
                };
                let pointer = child_pointer(&pointer, name);
                match (variant, body) {
                    (EnumVariant::Tuple { fields, .. }, body) if fields.len() == 1 => {
                        children.push((&fields[0], body, pointer));
                    }
                    (EnumVariant::Tuple { fields, .. }, Value::Array(items)) => {
                        for (index, (ty, item)) in fields.iter().zip(items).enumerate() {
                            children.push((ty, item, child_pointer(&pointer, &index.to_string())));
                        }
                    }
                    (EnumVariant::Struct { fields, .. }, Value::Object(object)) => {
                        for field in fields.iter() {
                            if let Some(child) = object.get(field.name.as_ref()) {
                                children.push((&field.ty, child, child_pointer(&pointer, &field.name)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            (SchemaType::Option(inner), value) if !value.is_null() => children.push((&**inner, value, pointer)),
            (SchemaType::Vec(element) | SchemaType::Array { element, .. }, Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    children.push((&**element, item, child_pointer(&pointer, &index.to_string())));
                }
            }
            (SchemaType::Map { value: element, .. }, Value::Object(object)) => {
                for (key, item) in object {
                    children.push((&**element, item, child_pointer(&pointer, key)));
                }
            }
            _ => {}
        }
        pending.extend(children.into_iter().rev());
    }
    for (pointer, digest) in &found {
        if let Some(slot) = json.pointer_mut(pointer) {
            *slot = Value::String(digest.to_string());
        }
    }
    found
}

/// The digest `value` holds when `schema` is `[u8; 32]` and `value` is 32
/// bytes.
fn digest_at(schema: &SchemaType, value: &Value) -> Option<Digest> {
    let SchemaType::Array { element, len: 32 } = schema else {
        return None;
    };
    if !matches!(**element, SchemaType::Scalar(Primitive::U8)) {
        return None;
    }
    let Value::Array(items) = value else {
        return None;
    };
    let mut bytes = [0; 32];
    for (slot, item) in bytes.iter_mut().zip(items) {
        *slot = u8::try_from(item.as_u64()?).ok()?;
    }
    (items.len() == 32).then(|| Digest::from_bytes(bytes))
}

/// `pointer` extended by one reference token, escaped per RFC 6901.
pub fn child_pointer(pointer: &str, token: &str) -> String {
    format!("{pointer}/{}", token.replace('~', "~0").replace('/', "~1"))
}
