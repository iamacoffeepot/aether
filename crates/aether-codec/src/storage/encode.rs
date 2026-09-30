//! JSON to storage records: the schema-side `StorageLeaves::contribute` and
//! `StorageElement::contribute_element`.

use aether_data::storage::{
    RecordWriter, U64_SCHEMA, VARIANT_LEAF, field_path_root, fold_index_segment, fold_path_segment,
    terminate_field_hash_runtime, variant_hash_runtime,
};
use aether_data::{EnumVariant, NamedField, SchemaType};
use serde_json::{Map, Value};

use super::{container_hash, tagged, too_deep, variant_discriminant};
use crate::EncodeError;
use crate::encode::{decode_enum_tag, encode_wire_value, parse_map_key};

pub(super) fn encode(value: &Value, schema: &SchemaType) -> Result<Vec<u8>, EncodeError> {
    let mut sink = RecordWriter::new();
    leaves(value, schema, field_path_root(), 0, "$", &mut sink)?;
    finish(sink, "$")
}

/// Write every leaf of `value` under `carry`, the way the derived
/// `contribute` does for the Rust type `schema` describes.
fn leaves(
    value: &Value,
    schema: &SchemaType,
    carry: u64,
    depth: u32,
    path: &str,
    sink: &mut RecordWriter,
) -> Result<(), EncodeError> {
    if too_deep(depth) {
        return Err(EncodeError::UnsupportedSchema("value nests deeper than the storage depth cap"));
    }
    match schema {
        SchemaType::Unit
        | SchemaType::Bool
        | SchemaType::Scalar(_)
        | SchemaType::String
        | SchemaType::Bytes
        | SchemaType::TypeId(_) => {
            let mut body = Vec::new();
            encode_wire_value(value, schema, path, &mut body)?;
            emit(sink, terminate_field_hash_runtime(carry, schema), body)
        }
        SchemaType::Struct { repr_c: true, .. } => {
            Err(EncodeError::UnsupportedSchema("a repr(C) struct has no flattened storage form"))
        }
        SchemaType::Struct { fields, repr_c: false } => {
            named_leaves(object(value, path)?, fields, carry, depth, path, sink)
        }
        SchemaType::Enum { variants } => enum_leaves(value, variants, carry, depth, path, sink),
        SchemaType::Option(inner) => {
            let discriminant_hash =
                terminate_field_hash_runtime(fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth), &U64_SCHEMA);
            if value.is_null() {
                return emit_discriminant(sink, discriminant_hash, variant_hash_runtime("None", &SchemaType::Unit));
            }
            emit_discriminant(sink, discriminant_hash, variant_hash_runtime("Some", inner))?;
            leaves(value, inner, fold_path_segment(carry, b"Some", depth), depth + 1, path, sink)
        }
        SchemaType::Vec(_) | SchemaType::Array { .. } | SchemaType::Map { .. } => {
            let mut body = Vec::new();
            element(value, schema, depth, path, &mut body)?;
            emit(sink, container_hash(carry, depth, schema), body)
        }
        SchemaType::Blob => Err(EncodeError::UnsupportedSchema("a blob has no storage form")),
        SchemaType::Ticket { .. } => Err(EncodeError::ActorReach { field: path.to_owned() }),
    }
}

/// The fields of a struct, or of a struct variant, each folded onto `carry`
/// at `depth` and walked one level deeper. An absent `Option` field is
/// `None`, as in the wire encoder.
fn named_leaves(
    object: &Map<String, Value>,
    fields: &[NamedField],
    carry: u64,
    depth: u32,
    path: &str,
    sink: &mut RecordWriter,
) -> Result<(), EncodeError> {
    if let Some(key) = object.keys().find(|key| !fields.iter().any(|field| field.name == **key)) {
        return Err(EncodeError::UnexpectedField(format!("{path}.{key}")));
    }
    for field in fields {
        let field_path = format!("{path}.{}", field.name);
        let value = match (object.get(&*field.name), &field.ty) {
            (Some(value), _) => value,
            (None, SchemaType::Option(_)) => &Value::Null,
            (None, _) => return Err(EncodeError::MissingField(field_path)),
        };
        let child = fold_path_segment(carry, field.name.as_bytes(), depth);
        leaves(value, &field.ty, child, depth + 1, &field_path, sink)?;
    }
    Ok(())
}

fn enum_leaves(
    value: &Value,
    variants: &[EnumVariant],
    carry: u64,
    depth: u32,
    path: &str,
    sink: &mut RecordWriter,
) -> Result<(), EncodeError> {
    // The derive writes the discriminant as a `u64` leaf one level down.
    if too_deep(depth + 1) {
        return Err(EncodeError::UnsupportedSchema("value nests deeper than the storage depth cap"));
    }
    let (tag, body) = decode_enum_tag(value, path)?;
    let variant = variants.iter().find(|variant| variant.name() == tag).ok_or_else(|| EncodeError::TypeMismatch {
        field: path.to_owned(),
        expected: "enum variant matching schema",
    })?;

    let discriminant_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
    emit_discriminant(
        sink,
        terminate_field_hash_runtime(discriminant_carry, &U64_SCHEMA),
        variant_discriminant(variant),
    )?;

    let body_carry = fold_path_segment(carry, tag.as_bytes(), depth);
    match variant {
        EnumVariant::Unit { .. } if body.is_null() => Ok(()),
        EnumVariant::Unit { .. } => Err(EncodeError::TypeMismatch {
            field: path.to_owned(),
            expected: "unit variant has no body — pass the variant name as a bare string",
        }),
        EnumVariant::Tuple { fields, .. } if fields.len() == 1 => {
            leaves(body, &fields[0], body_carry, depth + 1, &format!("{path}::{tag}.0"), sink)
        }
        EnumVariant::Tuple { fields, .. } => {
            let items = body.as_array().ok_or_else(|| EncodeError::TypeMismatch {
                field: path.to_owned(),
                expected: "tuple variant body as array",
            })?;
            if items.len() != fields.len() {
                return Err(EncodeError::ArrayLengthMismatch {
                    field: path.to_owned(),
                    expected: u32::try_from(fields.len()).unwrap_or(u32::MAX),
                    got: items.len(),
                });
            }
            for (index, (item, ty)) in items.iter().zip(fields.iter()).enumerate() {
                let child = fold_index_segment(body_carry, depth + 1, index);
                leaves(item, ty, child, depth + 2, &format!("{path}::{tag}.{index}"), sink)?;
            }
            Ok(())
        }
        EnumVariant::Struct { fields, .. } => {
            let body_path = format!("{path}::{tag}");
            named_leaves(object(body, &body_path)?, fields, body_carry, depth + 1, &body_path, sink)
        }
    }
}

/// One container element's body, the schema-side `contribute_element`: a
/// positional element is its wire bytes, a flattened type is a `u32` length
/// then its own record stream rooted afresh, and a container of tagged
/// elements frames each one.
fn element(value: &Value, schema: &SchemaType, depth: u32, path: &str, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    if !tagged(schema) {
        return encode_wire_value(value, schema, path, out);
    }
    match schema {
        SchemaType::Option(inner) => {
            if value.is_null() {
                out.push(0);
                return Ok(());
            }
            out.push(1);
            element(value, inner, depth, path, out)
        }
        SchemaType::Vec(inner) => {
            let items = array(value, path)?;
            write_count(out, items.len(), path)?;
            for (index, item) in items.iter().enumerate() {
                element(item, inner, depth, &format!("{path}[{index}]"), out)?;
            }
            Ok(())
        }
        SchemaType::Array { element: inner, len } => {
            let items = array(value, path)?;
            if items.len() != *len as usize {
                return Err(EncodeError::ArrayLengthMismatch {
                    field: path.to_owned(),
                    expected: *len,
                    got: items.len(),
                });
            }
            for (index, item) in items.iter().enumerate() {
                element(item, inner, depth, &format!("{path}[{index}]"), out)?;
            }
            Ok(())
        }
        SchemaType::Map { key: key_schema, value: value_schema } => {
            // Ascending encoded-key order, as the derived map element and
            // the wire codec both sort.
            let mut entries = Vec::new();
            for (key, item) in object(value, path)? {
                let entry_path = format!("{path}.{key}");
                let mut key_bytes = Vec::new();
                element(&parse_map_key(key, key_schema, &entry_path)?, key_schema, depth, &entry_path, &mut key_bytes)?;
                let mut value_bytes = Vec::new();
                element(item, value_schema, depth, &entry_path, &mut value_bytes)?;
                entries.push((key_bytes, value_bytes));
            }
            entries.sort_by(|left, right| left.0.cmp(&right.0));

            write_count(out, entries.len(), path)?;
            for (key_bytes, value_bytes) in entries {
                out.extend_from_slice(&key_bytes);
                out.extend_from_slice(&value_bytes);
            }
            Ok(())
        }
        SchemaType::Struct { .. } | SchemaType::Enum { .. } => {
            let mut stream = RecordWriter::new();
            leaves(value, schema, field_path_root(), depth, path, &mut stream)?;
            let stream = finish(stream, path)?;
            write_count(out, stream.len(), path)?;
            out.extend_from_slice(&stream);
            Ok(())
        }
        _ => encode_wire_value(value, schema, path, out),
    }
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, EncodeError> {
    value.as_object().ok_or_else(|| EncodeError::TypeMismatch { field: path.to_owned(), expected: "object" })
}

fn array<'a>(value: &'a Value, path: &str) -> Result<&'a Vec<Value>, EncodeError> {
    value.as_array().ok_or_else(|| EncodeError::TypeMismatch { field: path.to_owned(), expected: "array" })
}

fn write_count(out: &mut Vec<u8>, len: usize, path: &str) -> Result<(), EncodeError> {
    let count = u32::try_from(len)
        .map_err(|_| EncodeError::OutOfRange { field: path.to_owned(), reason: "length exceeds u32".into() })?;
    out.extend_from_slice(&count.to_le_bytes());
    Ok(())
}

fn emit_discriminant(sink: &mut RecordWriter, hash: u64, discriminant: u64) -> Result<(), EncodeError> {
    emit(sink, hash, discriminant.to_le_bytes().to_vec())
}

fn emit(sink: &mut RecordWriter, hash: u64, body: Vec<u8>) -> Result<(), EncodeError> {
    // The derive proves its leaf tags distinct at compile time; a schema
    // that arrives as data can still collide.
    sink.emit(hash, body).map_err(|_| EncodeError::UnsupportedSchema("two leaves of the schema share one record tag"))
}

fn finish(sink: RecordWriter, path: &str) -> Result<Vec<u8>, EncodeError> {
    sink.finish().map_err(|error| EncodeError::OutOfRange { field: path.to_owned(), reason: error.to_string() })
}
