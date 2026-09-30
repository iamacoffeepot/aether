//! `json_schema`: a `SchemaType` and its doc tree → the JSON Schema of the
//! JSON [`crate::encode_schema`] accepts for it, each field and variant
//! carrying its `///` doc as `description`.
//!
//! The structure is exact, arm by arm, to the encoder:
//!
//! - a struct is an object with `additionalProperties: false`, requiring
//!   every field but an `Option` one, whose absence encodes as `None`;
//! - an enum is externally tagged, one `oneOf` branch per variant: a unit
//!   variant is `"Name"` or `{"Name": null}`, a struct variant
//!   `{"Name": {..}}`, a one-field tuple variant `{"Name": value}` (or a
//!   bare `"Name"` when that field accepts null), and any other tuple
//!   variant `{"Name": [..]}` of exact length;
//! - `Bytes` and `Blob` are arrays of integers `0..=255`, and `[T; N]` an
//!   array of exactly `N` items;
//! - integers carry their primitive's exact bounds, and floats are numbers;
//! - a typed id is a tagged-id string or a non-negative integer, and a map
//!   is an object whose property names follow its key schema's string form.
//!
//! Value-level rules stay the decoder's: a validated newtype's check, a
//! typed id's tag, `1.0` offered for an integer (JSON Schema's `integer`
//! admits it), and an integer map key's width.

use std::error;
use std::fmt;

use aether_data::{Doc, DocNode, EnumVariant, FieldDoc, NamedField, Primitive, SchemaType, VariantDoc};
use serde_json::{Map, Value, json};

use crate::MAX_SCHEMA_DEPTH;

/// Why a schema has no JSON Schema form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonSchemaError {
    /// A held-reply ticket, which no outside caller may write (ADR-0243).
    ActorReach { field: String },
    /// The doc tree does not mirror a struct or enum in the schema.
    DocShapeMismatch { field: String },
    /// The schema nests deeper than [`MAX_SCHEMA_DEPTH`].
    TooDeep,
}

impl fmt::Display for JsonSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActorReach { field } => {
                write!(f, "field {field:?} is a held-reply ticket, which never crosses outside its actor")
            }
            Self::DocShapeMismatch { field } => write!(f, "field {field:?}: the doc tree does not mirror the schema"),
            Self::TooDeep => write!(f, "schema nests deeper than {MAX_SCHEMA_DEPTH}"),
        }
    }
}

impl error::Error for JsonSchemaError {}

/// The JSON Schema of the JSON [`crate::encode_schema`] accepts for
/// `schema`, with the docs in `docs` attached as `description`s.
///
/// # Errors
///
/// [`JsonSchemaError`] for a ticket field, a doc tree that does not mirror a
/// struct or enum, or nesting past [`MAX_SCHEMA_DEPTH`].
pub fn json_schema(schema: &SchemaType, docs: &DocNode) -> Result<Value, JsonSchemaError> {
    render(schema, Some(docs), "$", 0)
}

/// `docs` is `None` where the tree supplies nothing, which a struct or enum
/// refuses.
fn render(schema: &SchemaType, docs: Option<&DocNode>, path: &str, depth: usize) -> Result<Value, JsonSchemaError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(JsonSchemaError::TooDeep);
    }
    let next = depth + 1;
    Ok(match schema {
        SchemaType::Unit => json!({}),
        SchemaType::Bool => json!({ "type": "boolean" }),
        SchemaType::Scalar(primitive) => scalar(*primitive),
        SchemaType::String => json!({ "type": "string" }),
        SchemaType::Bytes | SchemaType::Blob => {
            json!({ "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 255 } })
        }
        SchemaType::Option(inner) => {
            let inner = render(inner, inner_docs(docs, DocArm::Option), path, next)?;
            json!({ "anyOf": [{ "type": "null" }, inner] })
        }
        SchemaType::Vec(inner) => {
            let items = render(inner, inner_docs(docs, DocArm::Vec), &format!("{path}[]"), next)?;
            json!({ "type": "array", "items": items })
        }
        SchemaType::Array { element, len } => {
            let items = render(element, inner_docs(docs, DocArm::Array), &format!("{path}[]"), next)?;
            json!({ "type": "array", "items": items, "minItems": len, "maxItems": len })
        }
        SchemaType::Struct { fields, .. } => {
            let Some(DocNode::Struct { fields: field_docs }) = docs else {
                return Err(mismatch(path));
            };
            object(fields, field_docs, path, next)?
        }
        SchemaType::Enum { variants } => {
            let Some(DocNode::Enum { variants: variant_docs }) = docs else {
                return Err(mismatch(path));
            };
            if variants.len() != variant_docs.len() {
                return Err(mismatch(path));
            }
            if variants.is_empty() {
                return Ok(json!({ "not": {} }));
            }
            let branches = variants
                .iter()
                .zip(variant_docs.iter())
                .map(|(variant, variant_doc)| enum_branch(variant, variant_doc, path, next))
                .collect::<Result<Vec<_>, _>>()?;
            json!({ "oneOf": branches })
        }
        SchemaType::Map { key, value } => {
            let (key_docs, value_docs) = match docs {
                Some(DocNode::Map { key, value }) => (Some(&**key), Some(&**value)),
                _ => (None, None),
            };
            let values = render(value, value_docs, &format!("{path}{{}}"), next)?;
            let mut map = json!({ "type": "object", "additionalProperties": values });
            if let Some(names) = map_key_names(key, key_docs, path)? {
                map["propertyNames"] = names;
            }
            map
        }
        SchemaType::TypeId(type_id) => {
            if aether_data::tag_for_type_id(*type_id).is_none() {
                // The encoder knows no tag for this id and refuses every value.
                json!({ "not": {} })
            } else {
                json!({ "anyOf": [{ "type": "string" }, { "type": "integer", "minimum": 0, "maximum": u64::MAX }] })
            }
        }
        SchemaType::Ticket { .. } => return Err(JsonSchemaError::ActorReach { field: path.to_owned() }),
    })
}

enum DocArm {
    Option,
    Vec,
    Array,
}

/// The element tree of a container's docs, `None` when they do not mirror
/// the container.
fn inner_docs(docs: Option<&DocNode>, arm: DocArm) -> Option<&DocNode> {
    match (docs?, arm) {
        (DocNode::Option(cell), DocArm::Option)
        | (DocNode::Vec(cell), DocArm::Vec)
        | (DocNode::Array(cell), DocArm::Array) => Some(&**cell),
        _ => None,
    }
}

fn mismatch(path: &str) -> JsonSchemaError {
    JsonSchemaError::DocShapeMismatch { field: path.to_owned() }
}

fn scalar(primitive: Primitive) -> Value {
    match primitive {
        Primitive::U8 => integer(u8::MIN, u8::MAX),
        Primitive::U16 => integer(u16::MIN, u16::MAX),
        Primitive::U32 => integer(u32::MIN, u32::MAX),
        Primitive::U64 => integer(u64::MIN, u64::MAX),
        Primitive::I8 => integer(i8::MIN, i8::MAX),
        Primitive::I16 => integer(i16::MIN, i16::MAX),
        Primitive::I32 => integer(i32::MIN, i32::MAX),
        Primitive::I64 => integer(i64::MIN, i64::MAX),
        Primitive::F32 | Primitive::F64 => json!({ "type": "number" }),
    }
}

fn integer(minimum: impl Into<Value>, maximum: impl Into<Value>) -> Value {
    json!({ "type": "integer", "minimum": minimum.into(), "maximum": maximum.into() })
}

/// A struct, or a struct variant's body: every field a property carrying its
/// doc, every non-`Option` field required, no other key.
fn object(fields: &[NamedField], field_docs: &[FieldDoc], path: &str, depth: usize) -> Result<Value, JsonSchemaError> {
    if fields.len() != field_docs.len() {
        return Err(mismatch(path));
    }
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (field, field_doc) in fields.iter().zip(field_docs) {
        let field_path = format!("{path}.{}", field.name);
        let property = described(render(&field.ty, Some(&field_doc.node), &field_path, depth)?, &field_doc.doc);
        properties.insert(field.name.to_string(), property);
        if !matches!(field.ty, SchemaType::Option(_)) {
            required.push(Value::String(field.name.to_string()));
        }
    }
    Ok(json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    }))
}

/// One `oneOf` branch: the variant's JSON forms, carrying its doc.
fn enum_branch(
    variant: &EnumVariant,
    variant_doc: &VariantDoc,
    path: &str,
    depth: usize,
) -> Result<Value, JsonSchemaError> {
    let name = variant.name();
    let variant_path = format!("{path}::{name}");
    let branch = match variant {
        EnumVariant::Unit { .. } => json!({ "anyOf": [{ "const": name }, tagged(name, json!({ "type": "null" }))] }),
        EnumVariant::Tuple { fields, .. } => {
            if fields.len() != variant_doc.fields.len() {
                return Err(mismatch(&variant_path));
            }
            let items = fields
                .iter()
                .zip(variant_doc.fields.iter())
                .enumerate()
                .map(|(index, (ty, field_doc))| {
                    let field_path = format!("{variant_path}.{index}");
                    render(ty, Some(&field_doc.node), &field_path, depth)
                })
                .collect::<Result<Vec<_>, _>>()?;
            match <[Value; 1]>::try_from(items) {
                // A bare `"Name"` reaches the encoder as a null body, which
                // a lone field that accepts null takes.
                Ok([only]) if matches!(fields[0], SchemaType::Option(_) | SchemaType::Unit) => {
                    json!({ "anyOf": [{ "const": name }, tagged(name, only)] })
                }
                Ok([only]) => tagged(name, only),
                Err(items) => {
                    let len = items.len();
                    tagged(name, json!({ "type": "array", "prefixItems": items, "minItems": len, "maxItems": len }))
                }
            }
        }
        EnumVariant::Struct { fields, .. } => tagged(name, object(fields, &variant_doc.fields, &variant_path, depth)?),
    };
    Ok(described(branch, &variant_doc.doc))
}

/// `{"name": body}` and nothing else.
fn tagged(name: &str, body: Value) -> Value {
    let mut properties = Map::new();
    properties.insert(name.to_owned(), body);
    json!({
        "type": "object",
        "properties": properties,
        "required": [name],
        "additionalProperties": false,
    })
}

/// `schema` with `doc` as its `description`, when it has one.
fn described(mut schema: Value, doc: &Doc) -> Value {
    if let (Doc::Written(text), Value::Object(object)) = (doc, &mut schema)
        && !text.is_empty()
    {
        object.insert("description".to_owned(), Value::String(text.to_string()));
    }
    schema
}

/// The `propertyNames` rule for a map's keys: the string form the encoder
/// parses each key from. `None` for a string key, which admits any name;
/// `false` for a key the encoder never accepts, leaving only the empty map.
fn map_key_names(key: &SchemaType, key_docs: Option<&DocNode>, path: &str) -> Result<Option<Value>, JsonSchemaError> {
    Ok(match key {
        SchemaType::String => None,
        SchemaType::Bool => Some(json!({ "enum": ["true", "false"] })),
        SchemaType::Scalar(Primitive::U8 | Primitive::U16 | Primitive::U32 | Primitive::U64) => {
            Some(json!({ "pattern": "^\\+?[0-9]+$" }))
        }
        SchemaType::Scalar(Primitive::I8 | Primitive::I16 | Primitive::I32 | Primitive::I64) => {
            Some(json!({ "pattern": "^[+-]?[0-9]+$" }))
        }
        SchemaType::Enum { variants } => {
            if !matches!(key_docs, Some(DocNode::Enum { .. })) {
                return Err(mismatch(&format!("{path}{{key}}")));
            }
            let names: Vec<&str> = variants
                .iter()
                .filter(|variant| matches!(variant, EnumVariant::Unit { .. }))
                .map(EnumVariant::name)
                .collect();
            Some(json!({ "enum": names }))
        }
        _ => Some(Value::Bool(false)),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aether_data::{KindId, Schema};
    use jsonschema::draft202012;
    use serde_json::{Value, json};

    use super::json_schema;
    use crate::encode_schema;

    /// How to run.
    #[derive(aether_data::Schema)]
    enum Mode {
        /// As fast as possible.
        Fast,
        /// One count.
        One(u32),
        /// Maybe a level.
        Maybe(Option<u8>),
        /// A level and a label.
        Pair(u8, String),
        /// Nested settings.
        Named {
            /// How deep to go.
            depth: i8,
            /// What to call it.
            label: Option<String>,
        },
    }

    /// A map key.
    #[derive(aether_data::Schema, PartialEq, Eq, PartialOrd, Ord)]
    enum Key {
        /// The first key.
        A,
        /// The second key.
        B,
    }

    /// Every schema arm a program input can reach.
    #[derive(aether_data::Schema)]
    struct Composite {
        /// An optional note.
        note: Option<String>,
        /// The run mode.
        mode: Mode,
        /// Modes to try next.
        modes: Vec<Mode>,
        /// Raw bytes.
        raw: Vec<u8>,
        /// A fixed digest.
        digest: [u8; 4],
        /// A u8.
        a_u8: u8,
        /// A u16.
        a_u16: u16,
        /// A u32.
        a_u32: u32,
        /// A u64.
        a_u64: u64,
        /// An i8.
        a_i8: i8,
        /// An i16.
        a_i16: i16,
        /// An i32.
        a_i32: i32,
        /// An i64.
        a_i64: i64,
        /// A ratio.
        ratio: f32,
        /// A flag.
        flag: bool,
        /// Counts by name.
        by_name: BTreeMap<String, u32>,
        /// Flags by index.
        by_index: BTreeMap<u16, bool>,
        /// Offsets by key.
        by_key: BTreeMap<Key, i32>,
        /// A kind id.
        kind: KindId,
    }

    fn base() -> Value {
        json!({
            "note": "hi",
            "mode": "Fast",
            "modes": ["Fast", { "One": 1 }],
            "raw": [0, 255],
            "digest": [1, 2, 3, 4],
            "a_u8": 255,
            "a_u16": 65535,
            "a_u32": 4_294_967_295_u64,
            "a_u64": u64::MAX,
            "a_i8": -128,
            "a_i16": -32768,
            "a_i32": -2_147_483_648_i64,
            "a_i64": i64::MIN,
            "ratio": 0.5,
            "flag": true,
            "by_name": { "x": 1 },
            "by_index": { "7": true, "+8": false },
            "by_key": { "A": -1 },
            "kind": 5,
        })
    }

    fn with(key: &str, value: Value) -> Value {
        let mut sample = base();
        sample[key] = value;
        sample
    }

    fn without(key: &str) -> Value {
        let mut sample = base();
        sample.as_object_mut().expect("an object").remove(key);
        sample
    }

    #[test]
    fn the_rendered_schema_accepts_exactly_what_the_encoder_accepts() {
        // Catches a renderer that describes a shape the codec refuses (adjacent tagging, base64 bytes, an optional
        // field marked required) or admits one it refuses.
        let schema = json_schema(&Composite::SCHEMA, &Composite::DOC_NODE).expect("renders");
        let validator = draft202012::new(&schema).expect("a valid JSON Schema");
        let samples = [
            base(),
            without("note"),
            with("note", Value::Null),
            with("note", json!(3)),
            without("flag"),
            with("extra", json!(1)),
            with("mode", json!({ "Fast": null })),
            with("mode", json!({ "Fast": 1 })),
            with("mode", json!({ "One": 7 })),
            with("mode", json!({ "One": [7] })),
            with("mode", json!("One")),
            with("mode", json!("Maybe")),
            with("mode", json!({ "Maybe": null })),
            with("mode", json!({ "Maybe": 3 })),
            with("mode", json!({ "Maybe": 300 })),
            with("mode", json!({ "Pair": [1, "s"] })),
            with("mode", json!({ "Pair": [1] })),
            with("mode", json!({ "Pair": [1, "s", 2] })),
            with("mode", json!({ "Named": { "depth": -3 } })),
            with("mode", json!({ "Named": { "depth": 1, "label": null } })),
            with("mode", json!({ "Named": { "depth": 1, "extra": 0 } })),
            with("mode", json!({ "Named": {} })),
            with("mode", json!("Named")),
            with("mode", json!("Nope")),
            with("mode", json!({ "Fast": null, "One": 1 })),
            with("modes", json!([])),
            with("modes", json!([3])),
            with("raw", json!([])),
            with("raw", json!([256])),
            with("raw", json!([-1])),
            with("raw", json!("AAE=")),
            with("digest", json!([1, 2, 3])),
            with("digest", json!([1, 2, 3, 4, 5])),
            with("a_u8", json!(256)),
            with("a_u8", json!(-1)),
            with("a_u16", json!(65536)),
            with("a_u32", json!(4_294_967_296_u64)),
            with("a_u32", json!(1.5)),
            with("a_u64", json!(-1)),
            with("a_i8", json!(-129)),
            with("a_i8", json!(128)),
            with("a_i16", json!(32768)),
            with("a_i32", json!(2_147_483_648_u64)),
            with("a_i64", json!(u64::MAX)),
            with("ratio", json!(3)),
            with("ratio", json!("x")),
            with("flag", json!("true")),
            with("by_name", json!({})),
            with("by_name", json!({ "x": -1 })),
            with("by_index", json!({ "x": true })),
            with("by_index", json!({ "-1": true })),
            with("by_key", json!({ "B": 2 })),
            with("by_key", json!({ "C": 1 })),
            with("kind", json!(-1)),
            with("kind", json!(true)),
        ];
        for sample in samples {
            let accepted = encode_schema(&sample, &Composite::SCHEMA).is_ok();
            assert_eq!(validator.is_valid(&sample), accepted, "sample {sample}");
        }
    }

    #[test]
    fn field_and_variant_docs_become_descriptions() {
        // Catches a renderer that drops docs or hangs them on the wrong property or branch.
        let schema = json_schema(&Composite::SCHEMA, &Composite::DOC_NODE).expect("renders");
        assert_eq!(schema["properties"]["note"]["description"], "An optional note.");
        assert_eq!(schema["properties"]["mode"]["oneOf"][2]["description"], "Maybe a level.");
        let named = &schema["properties"]["mode"]["oneOf"][4]["properties"]["Named"];
        assert_eq!(named["properties"]["depth"]["description"], "How deep to go.");
    }
}
