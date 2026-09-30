//! Storage records to JSON: the schema-side `StorageLeaves::assemble` and
//! `StorageElement::assemble_element`, strict about what the schema does
//! not bind.

use aether_data::storage::{
    RecordReader, U64_SCHEMA, VARIANT_LEAF, field_path_root, fold_index_segment, fold_path_segment,
    terminate_field_hash_runtime, variant_hash_runtime,
};
use aether_data::{EnumVariant, NamedField, SchemaType};
use serde_json::{Map, Value};

use super::{container_hash, tagged, too_deep, variant_discriminant};
use crate::DecodeError;
use crate::decode::{decode_wire_prefix_strict, render_map_key};

pub(super) fn decode(bytes: &[u8], schema: &SchemaType, maximum_values: usize) -> Result<Value, DecodeError> {
    let mut walk = Walk { budget: maximum_values, values_left: maximum_values };
    let mut records = parse(bytes, "$")?;
    let value = walk.leaves(schema, field_path_root(), 0, "$", &mut records)?;
    reject_rest(records, "$")?;
    Ok(value)
}

/// One decode's value ceiling, shared by the record walk and every wire
/// body it reads.
struct Walk {
    budget: usize,
    values_left: usize,
}

impl Walk {
    /// Rebuild the JSON for `schema` from the records under `carry`,
    /// taking each record it reads.
    fn leaves(
        &mut self,
        schema: &SchemaType,
        carry: u64,
        depth: u32,
        path: &str,
        records: &mut RecordReader,
    ) -> Result<Value, DecodeError> {
        if too_deep(depth) {
            return Err(DecodeError::UnsupportedSchema("value nests deeper than the storage depth cap"));
        }
        match schema {
            SchemaType::Unit
            | SchemaType::Bool
            | SchemaType::Scalar(_)
            | SchemaType::String
            | SchemaType::Bytes
            | SchemaType::TypeId(_) => {
                let body = take(records, terminate_field_hash_runtime(carry, schema), path)?;
                let mut cursor = body.as_slice();
                let value = self.wire(&mut cursor, schema, path)?;
                exhausted(cursor, path)?;
                Ok(value)
            }
            SchemaType::Struct { repr_c: true, .. } => {
                Err(DecodeError::UnsupportedSchema("a repr(C) struct has no flattened storage form"))
            }
            SchemaType::Struct { fields, repr_c: false } => self.named(fields, carry, depth, path, records),
            SchemaType::Enum { variants } => self.enumeration(variants, carry, depth, path, records),
            SchemaType::Option(inner) => {
                let discriminant_hash =
                    terminate_field_hash_runtime(fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth), &U64_SCHEMA);
                let Some(body) = records.take(discriminant_hash) else {
                    return self.null(path);
                };
                let discriminant = discriminant(&body, path)?;
                if discriminant == variant_hash_runtime("None", &SchemaType::Unit) {
                    self.null(path)
                } else if discriminant == variant_hash_runtime("Some", inner) {
                    self.leaves(inner, fold_path_segment(carry, b"Some", depth), depth + 1, path, records)
                } else {
                    Err(DecodeError::UnknownVariant { path: path.into(), hash: discriminant })
                }
            }
            SchemaType::Vec(_) | SchemaType::Array { .. } | SchemaType::Map { .. } => {
                let body = take(records, container_hash(carry, depth, schema), path)?;
                let mut cursor = body.as_slice();
                let value = self.element(schema, depth, path, &mut cursor)?;
                exhausted(cursor, path)?;
                Ok(value)
            }
            SchemaType::Blob => Err(DecodeError::UnsupportedSchema("a blob has no storage form")),
            SchemaType::Ticket { .. } => Err(DecodeError::ActorReach { path: path.into() }),
        }
    }

    /// The fields of a struct, or of a struct variant, each folded onto
    /// `carry` at `depth` and read one level deeper.
    fn named(
        &mut self,
        fields: &[NamedField],
        carry: u64,
        depth: u32,
        path: &str,
        records: &mut RecordReader,
    ) -> Result<Value, DecodeError> {
        self.charge(path)?;
        let mut object = Map::with_capacity(fields.len().min(self.values_left));
        for field in fields {
            let child = fold_path_segment(carry, field.name.as_bytes(), depth);
            let value = self.leaves(&field.ty, child, depth + 1, &format!("{path}.{}", field.name), records)?;
            object.insert(field.name.to_string(), value);
        }
        Ok(Value::Object(object))
    }

    fn enumeration(
        &mut self,
        variants: &[EnumVariant],
        carry: u64,
        depth: u32,
        path: &str,
        records: &mut RecordReader,
    ) -> Result<Value, DecodeError> {
        // The derive reads the discriminant as a `u64` leaf one level down.
        if too_deep(depth + 1) {
            return Err(DecodeError::UnsupportedSchema("value nests deeper than the storage depth cap"));
        }
        let discriminant_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        let discriminant_path = format!("{path}.{VARIANT_LEAF}");
        let body = take(records, terminate_field_hash_runtime(discriminant_carry, &U64_SCHEMA), &discriminant_path)?;
        let discriminant = discriminant(&body, &discriminant_path)?;
        let variant = variants
            .iter()
            .find(|variant| variant_discriminant(variant) == discriminant)
            .ok_or_else(|| DecodeError::UnknownVariant { path: path.into(), hash: discriminant })?;

        self.charge(path)?;
        let name = variant.name();
        let body_carry = fold_path_segment(carry, name.as_bytes(), depth);
        let body = match variant {
            EnumVariant::Unit { .. } => return Ok(Value::String(name.to_owned())),
            EnumVariant::Tuple { fields, .. } if fields.len() == 1 => {
                self.leaves(&fields[0], body_carry, depth + 1, &format!("{path}::{name}.0"), records)?
            }
            EnumVariant::Tuple { fields, .. } => {
                self.charge(path)?;
                let mut items = Vec::with_capacity(fields.len().min(self.values_left));
                for (index, ty) in fields.iter().enumerate() {
                    let child = fold_index_segment(body_carry, depth + 1, index);
                    items.push(self.leaves(ty, child, depth + 2, &format!("{path}::{name}.{index}"), records)?);
                }
                Value::Array(items)
            }
            EnumVariant::Struct { fields, .. } => {
                self.named(fields, body_carry, depth + 1, &format!("{path}::{name}"), records)?
            }
        };
        Ok(Value::Object(Map::from_iter([(name.to_owned(), body)])))
    }

    /// One container element from the front of `cursor`, the schema-side
    /// `assemble_element`.
    fn element(
        &mut self,
        schema: &SchemaType,
        depth: u32,
        path: &str,
        cursor: &mut &[u8],
    ) -> Result<Value, DecodeError> {
        if !tagged(schema) {
            return self.wire(cursor, schema, path);
        }
        match schema {
            SchemaType::Option(inner) => match take_bytes::<1>(cursor, path)? {
                [0] => self.null(path),
                [1] => self.element(inner, depth, path, cursor),
                [byte] => Err(DecodeError::InvalidBool { path: path.into(), byte }),
            },
            SchemaType::Vec(inner) => {
                self.charge(path)?;
                let count = count(cursor, path)?;
                let mut items = Vec::with_capacity(count.min(cursor.len()).min(self.values_left));
                for index in 0..count {
                    items.push(self.element(inner, depth, &format!("{path}[{index}]"), cursor)?);
                }
                Ok(Value::Array(items))
            }
            SchemaType::Array { element, len } => {
                self.charge(path)?;
                let mut items = Vec::with_capacity((*len as usize).min(self.values_left));
                for index in 0..*len {
                    items.push(self.element(element, depth, &format!("{path}[{index}]"), cursor)?);
                }
                Ok(Value::Array(items))
            }
            SchemaType::Map { key: key_schema, value: value_schema } => {
                self.charge(path)?;
                let count = count(cursor, path)?;
                let mut object = Map::with_capacity(count.min(cursor.len()).min(self.values_left));
                for index in 0..count {
                    let entry_path = format!("{path}[{index}]");
                    let key = self.element(key_schema, depth, &entry_path, cursor)?;
                    let value = self.element(value_schema, depth, &entry_path, cursor)?;
                    let key = render_map_key(&key, key_schema, &entry_path)?;
                    if object.contains_key(&key) {
                        return Err(DecodeError::DuplicateMapKey { path: entry_path });
                    }
                    object.insert(key, value);
                }
                Ok(Value::Object(object))
            }
            SchemaType::Struct { .. } | SchemaType::Enum { .. } => {
                let length = count(cursor, path)?;
                if cursor.len() < length {
                    return Err(DecodeError::Truncated { path: path.into(), needed: length, had: cursor.len() });
                }
                let (frame, rest) = cursor.split_at(length);
                *cursor = rest;
                let mut records = parse(frame, path)?;
                let value = self.leaves(schema, field_path_root(), depth, path, &mut records)?;
                reject_rest(records, path)?;
                Ok(value)
            }
            _ => self.wire(cursor, schema, path),
        }
    }

    fn wire(&mut self, cursor: &mut &[u8], schema: &SchemaType, path: &str) -> Result<Value, DecodeError> {
        decode_wire_prefix_strict(cursor, schema, path, self.budget, &mut self.values_left)
    }

    fn null(&mut self, path: &str) -> Result<Value, DecodeError> {
        self.charge(path)?;
        Ok(Value::Null)
    }

    fn charge(&mut self, path: &str) -> Result<(), DecodeError> {
        self.values_left = self
            .values_left
            .checked_sub(1)
            .ok_or_else(|| DecodeError::ValueBudgetExceeded { path: path.into(), budget: self.budget })?;
        Ok(())
    }
}

fn parse(bytes: &[u8], path: &str) -> Result<RecordReader, DecodeError> {
    RecordReader::parse(bytes).map_err(|error| DecodeError::Records { path: path.into(), error })
}

fn take(records: &mut RecordReader, hash: u64, path: &str) -> Result<Vec<u8>, DecodeError> {
    records.take(hash).ok_or_else(|| DecodeError::MissingRecord { path: path.into() })
}

/// Refuse the first record the walk left unread.
fn reject_rest(records: RecordReader, path: &str) -> Result<(), DecodeError> {
    records
        .into_unknown()
        .first()
        .map_or(Ok(()), |unbound| Err(DecodeError::UnboundRecord { path: path.into(), hash: unbound.hash }))
}

/// A `__variant` record body: exactly one little-endian `u64`.
fn discriminant(body: &[u8], path: &str) -> Result<u64, DecodeError> {
    let mut cursor = body;
    let discriminant = u64::from_le_bytes(take_bytes::<8>(&mut cursor, path)?);
    exhausted(cursor, path)?;
    Ok(discriminant)
}

fn count(cursor: &mut &[u8], path: &str) -> Result<usize, DecodeError> {
    Ok(u32::from_le_bytes(take_bytes::<4>(cursor, path)?) as usize)
}

fn take_bytes<const N: usize>(cursor: &mut &[u8], path: &str) -> Result<[u8; N], DecodeError> {
    let Some((head, rest)) = cursor.split_first_chunk::<N>() else {
        return Err(DecodeError::Truncated { path: path.into(), needed: N, had: cursor.len() });
    };
    *cursor = rest;
    Ok(*head)
}

fn exhausted(cursor: &[u8], path: &str) -> Result<(), DecodeError> {
    if cursor.is_empty() {
        Ok(())
    } else {
        Err(DecodeError::TrailingBytes { path: path.into(), remaining: cursor.len() })
    }
}
